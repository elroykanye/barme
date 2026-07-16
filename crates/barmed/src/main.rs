//! The barme server. Builds the engine and mounts both front doors on it.
//! Next: construct the engine, then serve the S3 and native routers.

fn main() {
    tracing_subscriber::fmt::init();
    tracing::info!("barmed: nothing wired up yet");
}
