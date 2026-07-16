//! The barme server. Builds the engine and mounts the front doors on it.
//! For now: open the engine and serve the S3 door.

use std::net::SocketAddr;
use std::sync::Arc;

use barme_engine::{Engine, Policy};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    // One engine, one policy for now; per-bucket policy lands later.
    let engine = Arc::new(Engine::open("./barme-data", Policy::default())?);

    let addr: SocketAddr = "0.0.0.0:9000".parse()?;
    tracing::info!("barmed: S3 door on {addr}");
    barme_s3::serve(engine, addr).await?;
    Ok(())
}
