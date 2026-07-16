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
use barme_semantic::{HttpEmbedder, MemoryIndex, Semantic};

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

    let s3_addr: SocketAddr = "0.0.0.0:9000".parse()?;
    let native_addr: SocketAddr = "0.0.0.0:7373".parse()?;
    tracing::info!("barmed: S3 on {s3_addr}, native on {native_addr}");

    let native_state = AppState {
        engine: engine.clone(),
        semantic,
    };

    tokio::try_join!(
        barme_s3::serve(engine.clone(), s3_addr),
        barme_native::serve(native_state, native_addr),
    )?;
    Ok(())
}
