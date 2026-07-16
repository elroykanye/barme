//! The barme server. Opens one engine and serves both front doors on it:
//! the S3 door for compatibility and the native door for everything S3 can't
//! say. They run on separate ports over the same engine.
//!
//! Semantic search is opt-in: set BARME_EMBED_URL (and optionally
//! BARME_EMBED_MODEL) to point at an embedder, and writes get indexed off the
//! write path by a background worker.

use std::net::SocketAddr;
use std::sync::Arc;

use barme_auth::Credentials;
use barme_engine::{Engine, Policy, WriteEvent};
use barme_native::AppState;
use barme_s3::S3State;
use barme_semantic::{HttpEmbedder, MemoryIndex, Semantic};

/// The embedded web console, compiled in only under the `ui` feature. The React
/// build output is baked into the binary and served with an SPA fallback.
#[cfg(feature = "ui")]
mod ui {
    use axum::{
        http::{header, Uri},
        response::{IntoResponse, Response},
        routing::get,
        Router,
    };
    use rust_embed::RustEmbed;

    #[derive(RustEmbed)]
    #[folder = "../../web/dist"]
    struct Assets;

    pub fn router() -> Router {
        Router::new().fallback(get(serve))
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
    tracing_subscriber::fmt::init();

    // One engine, one policy for now; per-bucket policy lands later.
    let mut engine = Engine::open("./barme-data", Policy::default())?;

    let semantic = match std::env::var("BARME_EMBED_URL") {
        Ok(url) => {
            let model = std::env::var("BARME_EMBED_MODEL").unwrap_or_default();
            let semantic = Arc::new(Semantic::new(
                Box::new(HttpEmbedder::new(url, model)),
                Box::new(MemoryIndex::new()),
            ));

            // Writes drop an event on this channel; a worker embeds them off the
            // request path so uploads never wait on the model.
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<WriteEvent>();
            engine.set_write_hook(move |ev| {
                let _ = tx.send(ev);
            });

            let worker = semantic.clone();
            tokio::spawn(async move {
                while let Some(ev) = rx.recv().await {
                    if let Err(e) = worker
                        .understand(&ev.tenant, ev.object_id, &ev.content_type, &ev.bytes)
                        .await
                    {
                        tracing::warn!("understand failed for {}: {e}", ev.object_id);
                    }
                }
            });

            tracing::info!("semantic search enabled");
            Some(semantic)
        }
        Err(_) => {
            tracing::info!("semantic search disabled (set BARME_EMBED_URL to enable)");
            None
        }
    };

    let engine = Arc::new(engine);

    let creds = Credentials::from_env().map(Arc::new);
    match &creds {
        Some(_) => tracing::info!("credentials loaded; auth enforced"),
        None => tracing::warn!(
            "no credentials set (BARME_ACCESS_KEY / BARME_SECRET_KEY); running open"
        ),
    }

    let s3_addr: SocketAddr = "0.0.0.0:9000".parse()?;
    let native_addr: SocketAddr = "0.0.0.0:7373".parse()?;
    tracing::info!("barmed: S3 on {s3_addr}, native on {native_addr}");

    let s3_state = S3State {
        engine: engine.clone(),
        creds: creds.clone(),
    };
    let native_state = AppState {
        engine: engine.clone(),
        semantic,
        creds,
    };

    let s3 = barme_s3::serve(s3_state, s3_addr);
    let native = barme_native::serve(native_state, native_addr);

    #[cfg(feature = "ui")]
    {
        let console_addr: SocketAddr = "0.0.0.0:7374".parse()?;
        tracing::info!("console on {console_addr}");
        let console = async move {
            let listener = tokio::net::TcpListener::bind(console_addr).await?;
            axum::serve(listener, ui::router()).await
        };
        tokio::try_join!(s3, native, console)?;
    }
    #[cfg(not(feature = "ui"))]
    {
        tokio::try_join!(s3, native)?;
    }
    Ok(())
}
