//! The barme server. Opens one engine and serves both front doors on it:
//! the S3 door for compatibility and the native door for everything S3 can't
//! say. They run on separate ports over the same engine.
//!
//! Semantic search is opt-in: set BARME_EMBED_URL (and optionally
//! BARME_EMBED_MODEL) to point at an embedder, and writes get indexed off the
//! write path by a background worker.

use std::net::SocketAddr;
use std::sync::Arc;

use barme_engine::{Engine, Policy, WriteEvent};
use barme_native::AppState;
use barme_s3::S3State;
use barme_semantic::{HttpEmbedder, MemoryIndex, Semantic};

/// Fan a write event out to webhooks and the semantic layer. Everything here is
/// best-effort: a failing hook or embedder is logged, never fatal, and never
/// blocks the write that produced the event (this runs on a worker task).
async fn dispatch_event(engine: &Arc<Engine>, semantic: &Option<Arc<Semantic>>, ev: WriteEvent) {
    // Webhooks: POST a small JSON event to every hook that wants "write".
    for hook in engine.list_webhooks().unwrap_or_default() {
        if !hook.wants("write") {
            continue;
        }
        let url = hook.url.clone();
        let payload = serde_json::json!({
            "event": "write",
            "object_id": ev.object_id.to_string(),
            "pot": ev.bucket,
            "key": ev.key,
            "content_type": ev.content_type,
        });
        tokio::spawn(async move {
            if let Err(e) = reqwest::Client::new().post(&url).json(&payload).send().await {
                tracing::warn!("webhook {url} failed: {e}");
            }
        });
    }

    // Semantic understanding + auto-tagging. The embedder may proxy back tags
    // and text; we store them as annotations on the object, best-effort.
    if let Some(semantic) = semantic {
        match semantic
            .understand(&ev.tenant, ev.object_id, &ev.content_type, &ev.bytes)
            .await
        {
            Ok(u) if !u.tags.is_empty() || u.text.is_some() => {
                if let Ok(mut ann) = engine.annotation(&ev.bucket, &ev.key) {
                    for (i, tag) in u.tags.iter().enumerate() {
                        ann.tags.insert(format!("auto:{i}"), tag.clone());
                    }
                    if let Some(text) = u.text {
                        if ann.note.is_empty() {
                            ann.note = text;
                        }
                    }
                    if let Err(e) = engine.set_annotation(&ev.bucket, &ev.key, &ann) {
                        tracing::warn!("auto-tag store failed for {}: {e}", ev.object_id);
                    }
                }
            }
            Ok(_) => {}
            Err(e) => tracing::warn!("understand failed for {}: {e}", ev.object_id),
        }
    }
}

/// Turn a bind address like `0.0.0.0:7375` into a public base URL. A wildcard
/// bind host is rewritten to localhost so the link is actually dialable.
fn http_base(addr: &str) -> String {
    let (host, port) = addr.rsplit_once(':').unwrap_or(("localhost", "7375"));
    let host = if host.is_empty() || host == "0.0.0.0" || host == "[::]" {
        "localhost"
    } else {
        host
    };
    format!("http://{host}:{port}")
}

/// The embedded web console, compiled in only under the `ui` feature. The React
/// build output is baked into the binary and served with an SPA fallback.
#[cfg(feature = "ui")]
mod ui {
    use axum::{
        http::{header, Uri},
        response::{IntoResponse, Response},
        Router,
    };
    use rust_embed::RustEmbed;

    #[derive(RustEmbed)]
    #[folder = "../../web/dist"]
    struct Assets;

    pub fn router() -> Router {
        Router::new().fallback(serve)
    }

    async fn serve(uri: Uri) -> Response {
        let path = uri.path().trim_start_matches('/');
        let path = if path.is_empty() { "index.html" } else { path };

        if let Some(file) = Assets::get(path) {
            let mime = mime_guess::from_path(path).first_or_octet_stream();
            return (
                [(header::CONTENT_TYPE, mime.as_ref())],
                file.data.into_owned(),
            )
                .into_response();
        }

        // Unknown path: hand back index.html so client-side routing works.
        match Assets::get("index.html") {
            Some(index) => (
                [(header::CONTENT_TYPE, "text/html")],
                index.data.into_owned(),
            )
                .into_response(),
            None => (axum::http::StatusCode::NOT_FOUND, "ui not built").into_response(),
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    use tracing_subscriber::EnvFilter;
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .init();

    let config = barme_config::Config::load()?;

    let policy = Policy {
        codec: config.default_policy.codec.clone(),
        zstd_level: config.default_policy.zstd_level,
        tenant: config.default_policy.tenant.clone(),
        policy_name: config.default_policy.policy_name.clone(),
    };
    let mut engine = Engine::open(&config.data_dir, policy)?;

    let semantic = match config.embed_url.clone() {
        Some(url) => {
            let model = config.embed_model.clone();
            tracing::info!("semantic search enabled");
            Some(Arc::new(Semantic::new(
                Box::new(HttpEmbedder::new(url, model)),
                Box::new(MemoryIndex::new()),
            )))
        }
        None => {
            tracing::info!("semantic search disabled (set embed_url to enable)");
            None
        }
    };

    // One event bus for both reactors. Writes drop an event here; a single
    // background worker fans it out to webhooks and (if configured) the semantic
    // layer, off the request path so uploads never wait.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<WriteEvent>();
    engine.set_write_hook(move |ev| {
        let _ = tx.send(ev);
    });

    let engine = Arc::new(engine);

    {
        let engine = engine.clone();
        let semantic = semantic.clone();
        tokio::spawn(async move {
            while let Some(ev) = rx.recv().await {
                dispatch_event(&engine, &semantic, ev).await;
            }
        });
    }

    // Seed the configured owner key on first run, then report the key count.
    if let Some(c) = &config.credentials {
        engine.ensure_owner(&c.access_key, &c.secret_key)?;
    }
    match engine.list_keys() {
        Ok(k) if !k.is_empty() => tracing::info!("{} access key(s); auth enforced", k.len()),
        _ => tracing::warn!("no access keys; running open"),
    }

    // Periodic garbage collection. Without this, deleted objects' chunks are
    // never reclaimed.
    {
        let engine = engine.clone();
        let grace = std::time::Duration::from_secs(config.gc_grace_secs);
        let interval_secs = config.gc_interval_secs.max(1);
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(std::time::Duration::from_secs(interval_secs));
            loop {
                ticker.tick().await;
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                if let Err(e) = engine.enforce_lifecycle(now) {
                    tracing::warn!("lifecycle pass failed: {e}");
                }
                match engine.gc_sweep(now, grace) {
                    Ok(s) if s.condemned > 0 || s.erased > 0 => {
                        tracing::info!(
                            "gc: {} condemned, {} erased, {} live",
                            s.condemned,
                            s.erased,
                            s.live
                        );
                    }
                    Ok(_) => {}
                    Err(e) => tracing::warn!("gc sweep failed: {e}"),
                }
            }
        });
    }

    let s3_addr: SocketAddr = config.s3_addr.parse()?;
    let native_addr: SocketAddr = config.native_addr.parse()?;
    let cdn_addr: SocketAddr = config.cdn_addr.parse()?;
    tracing::info!("barmed: S3 on {s3_addr}, native on {native_addr}, cdn on {cdn_addr}");

    let s3_state = S3State {
        engine: engine.clone(),
    };
    let native_state = AppState {
        engine: engine.clone(),
        semantic,
        cdn_base: http_base(&config.cdn_addr),
        started: std::time::Instant::now(),
    };

    let s3 = barme_s3::serve(s3_state, s3_addr);
    let native = barme_native::serve(native_state, native_addr);
    let cdn = barme_cdn::serve(engine.clone(), cdn_addr);

    #[cfg(feature = "ui")]
    {
        let console_addr: SocketAddr = config.console_addr.parse()?;
        tracing::info!("console on {console_addr}");
        let console = async move {
            let listener = tokio::net::TcpListener::bind(console_addr).await?;
            axum::serve(listener, ui::router()).await
        };
        tokio::try_join!(s3, native, cdn, console)?;
    }
    #[cfg(not(feature = "ui"))]
    {
        tokio::try_join!(s3, native, cdn)?;
    }
    Ok(())
}
