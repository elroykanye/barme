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
    // and text; we store them as annotations on the object, best-effort. The
    // event carries no bytes (so streaming writes stay flat in memory); read
    // the object back here, off the write path, only when indexing is on.
    if let Some(semantic) = semantic {
        let bytes = match engine.read_object(&ev.object_id) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!("read-back for understand failed for {}: {e}", ev.object_id);
                return;
            }
        };
        match semantic
            .understand(&ev.tenant, ev.object_id, &ev.content_type, &bytes)
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

/// Barme object store server.
#[derive(clap::Parser)]
#[command(name = "barmed", version, about = "Barme — a content-addressed object store")]
struct Cli {
    /// Config file to load (default: barme.toml, or $BARME_CONFIG)
    #[arg(long, value_name = "FILE")]
    config: Option<String>,
    /// Data directory (overrides config)
    #[arg(long, value_name = "DIR")]
    data_dir: Option<String>,
    /// Native API bind address, e.g. 0.0.0.0:7373
    #[arg(long)]
    native_addr: Option<String>,
    /// S3 API bind address
    #[arg(long)]
    s3_addr: Option<String>,
    /// CDN bind address
    #[arg(long)]
    cdn_addr: Option<String>,
    /// Web console bind address (with the `ui` feature)
    #[arg(long)]
    console_addr: Option<String>,
}

fn print_banner(config: &barme_config::Config, ui: bool) {
    const ART: &str = r"
     ___   __ _ _ _ _ __  ___
    | _ ) / _` | '_| '  \/ -_)
    |___/ \__,_|_| |_|_|_\___|
";
    println!("{ART}");
    println!(
        "  v{}  ·  content-addressed object store\n",
        env!("CARGO_PKG_VERSION")
    );
    if ui {
        println!("  console    {}", http_base(&config.console_addr));
    }
    println!("  API        {}", http_base(&config.native_addr));
    println!("  API docs   {}/docs", http_base(&config.native_addr));
    println!("  S3         {}", http_base(&config.s3_addr));
    println!("  CDN        {}", http_base(&config.cdn_addr));
    if let Some(c) = &config.credentials {
        if c.access_key == "barme" && c.secret_key == "barme" {
            println!("  login      barme / barme  (default — set BARME_ACCESS_KEY / BARME_SECRET_KEY)");
        } else {
            println!("  login      {} / (from config)", c.access_key);
        }
    }
    println!();
}

/// Bind `desired`, rolling forward to the next port if it's already taken, so a
/// stray old instance or a leftover WSL port relay doesn't stop barmed coming
/// up. Returns the listener actually bound.
async fn bind_with_fallback(
    desired: SocketAddr,
    label: &str,
) -> std::io::Result<tokio::net::TcpListener> {
    let mut addr = desired;
    for _ in 0..64 {
        match tokio::net::TcpListener::bind(addr).await {
            Ok(listener) => {
                if addr.port() != desired.port() {
                    tracing::warn!(
                        "{label}: port {} in use, bound {} instead",
                        desired.port(),
                        addr.port()
                    );
                }
                return Ok(listener);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => match addr.port().checked_add(1)
            {
                Some(p) => addr.set_port(p),
                None => break,
            },
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AddrInUse,
        format!("{label}: no free port found at or above {}", desired.port()),
    ))
}

// A generous worker count on purpose: request handlers call the engine's
// synchronous, filesystem-backed methods directly, so a burst of concurrent
// requests parks several workers in blocking I/O at once. With too few threads
// the accept loop itself stops being polled and every connection hangs after
// the TCP handshake. Sixteen leaves headroom for that burst on any platform.
#[tokio::main(flavor = "multi_thread", worker_threads = 16)]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    use clap::Parser;
    let cli = Cli::parse();
    if let Some(c) = &cli.config {
        std::env::set_var("BARME_CONFIG", c);
    }

    use tracing_subscriber::EnvFilter;
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .init();

    let mut config = barme_config::Config::load()?;
    if let Some(v) = cli.data_dir {
        config.data_dir = v;
    }
    if let Some(v) = cli.native_addr {
        config.native_addr = v;
    }
    if let Some(v) = cli.s3_addr {
        config.s3_addr = v;
    }
    if let Some(v) = cli.cdn_addr {
        config.cdn_addr = v;
    }
    if let Some(v) = cli.console_addr {
        config.console_addr = v;
    }

    let policy = Policy {
        codec: config.default_policy.codec.clone(),
        zstd_level: config.default_policy.zstd_level,
        tenant: config.default_policy.tenant.clone(),
        policy_name: config.default_policy.policy_name.clone(),
    };
    let mut engine = Engine::open(&config.data_dir, policy)?;
    if engine.recovered_temp() > 0 {
        tracing::warn!(
            "recovered from unclean shutdown: reaped {} temp file(s) from interrupted writes",
            engine.recovered_temp()
        );
    }

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
            // The first tick is immediate; consume it so we don't sweep the whole
            // data directory the instant we start serving requests.
            ticker.tick().await;
            loop {
                ticker.tick().await;
                let engine = engine.clone();
                // enforce_lifecycle and gc_sweep are synchronous filesystem walks.
                // Run them on the blocking pool so they never sit on an async
                // worker thread and starve the request handlers.
                let outcome = tokio::task::spawn_blocking(move || {
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs())
                        .unwrap_or(0);
                    if let Err(e) = engine.enforce_lifecycle(now) {
                        tracing::warn!("lifecycle pass failed: {e}");
                    }
                    engine.gc_sweep(now, grace)
                })
                .await;
                match outcome {
                    Ok(Ok(s)) if s.condemned > 0 || s.erased > 0 => {
                        tracing::info!(
                            "gc: {} condemned, {} erased, {} live",
                            s.condemned,
                            s.erased,
                            s.live
                        );
                    }
                    Ok(Ok(_)) => {}
                    Ok(Err(e)) => tracing::warn!("gc sweep failed: {e}"),
                    Err(e) => tracing::warn!("gc task failed: {e}"),
                }
            }
        });
    }

    // Bind every door up front, rolling to the next free port if one is taken,
    // then rewrite the config to what we actually bound so the banner and the
    // CDN base URL handed to the API reflect reality.
    let s3_listener = bind_with_fallback(config.s3_addr.parse()?, "S3").await?;
    let native_listener = bind_with_fallback(config.native_addr.parse()?, "API").await?;
    let cdn_listener = bind_with_fallback(config.cdn_addr.parse()?, "CDN").await?;
    config.s3_addr = s3_listener.local_addr()?.to_string();
    config.native_addr = native_listener.local_addr()?.to_string();
    config.cdn_addr = cdn_listener.local_addr()?.to_string();
    #[cfg(feature = "ui")]
    let console_listener = bind_with_fallback(config.console_addr.parse()?, "console").await?;
    #[cfg(feature = "ui")]
    {
        config.console_addr = console_listener.local_addr()?.to_string();
    }

    print_banner(&config, cfg!(feature = "ui"));
    tracing::info!(
        "barmed: S3 on {}, native on {}, cdn on {}",
        config.s3_addr,
        config.native_addr,
        config.cdn_addr
    );

    let s3_state = S3State {
        engine: engine.clone(),
        max_upload_bytes: config.max_upload_bytes,
    };
    let native_state = AppState {
        engine: engine.clone(),
        semantic,
        cdn_base: http_base(&config.cdn_addr),
        max_upload_bytes: config.max_upload_bytes,
        started: std::time::Instant::now(),
    };

    let s3 = barme_s3::serve(s3_state, s3_listener);
    let native = barme_native::serve(native_state, native_listener);
    let cdn = barme_cdn::serve(engine.clone(), cdn_listener);

    #[cfg(feature = "ui")]
    {
        let console = async move { axum::serve(console_listener, ui::router()).await };
        tokio::try_join!(s3, native, cdn, console)?;
    }
    #[cfg(not(feature = "ui"))]
    {
        tokio::try_join!(s3, native, cdn)?;
    }
    Ok(())
}
