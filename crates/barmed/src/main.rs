//! The barme server. Opens one engine and serves both front doors on it:
//! the S3 door for compatibility and the native door for everything S3 can't
//! say. They run on separate ports over the same engine.

use std::net::SocketAddr;
use std::sync::Arc;

use barme_engine::{Engine, Policy};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    // One engine, one policy for now; per-bucket policy lands later.
    let engine = Arc::new(Engine::open("./barme-data", Policy::default())?);

    let s3_addr: SocketAddr = "0.0.0.0:9000".parse()?;
    let native_addr: SocketAddr = "0.0.0.0:9001".parse()?;
    tracing::info!("barmed: S3 door on {s3_addr}, native door on {native_addr}");

    tokio::try_join!(
        barme_s3::serve(engine.clone(), s3_addr),
        barme_native::serve(engine, native_addr),
    )?;
    Ok(())
}
