//! Content-addressed storage. All IO lives here.
//!
//! Three things to persist:
//!   - chunks:    keyed by hash, written once, never mutated
//!   - manifests: keyed by object_id (also a hash), immutable
//!   - pointers:  bucket/key -> manifest hash, with history. The only mutable state.
//!
//! Write-then-reference is a hard rule enforced above this layer: chunks and
//! the manifest are durable before a pointer moves to them. GC leans on it to
//! know a just-written chunk is never garbage even before anything points at it.

mod chunk_store;
mod manifest_store;
mod pointer_store;

pub use chunk_store::ChunkStore;
pub use manifest_store::ManifestStore;
pub use pointer_store::PointerStore;

use barme_core::Hash;
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("serialize: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("bad hash: {0}")]
    Hash(#[from] barme_core::HashError),
    /// The bytes on disk don't match the address they were fetched by.
    /// This is corruption, caught on read instead of served silently.
    #[error("integrity: content at {addr} does not match its address")]
    Integrity { addr: String },
    #[error("invalid bucket name: {0:?}")]
    BadBucket(String),
}

pub type Result<T> = std::result::Result<T, StoreError>;

/// The whole store, rooted at one directory.
pub struct Store {
    pub chunks: ChunkStore,
    pub manifests: ManifestStore,
    pub pointers: PointerStore,
}

impl Store {
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref();
        Ok(Store {
            chunks: ChunkStore::open(root.join("chunks"))?,
            manifests: ManifestStore::open(root.join("manifests"))?,
            pointers: PointerStore::open(root.join("pointers"))?,
        })
    }
}

/// Git-style sharded path: `base/<first 2 hex>/<full hex>`. Keeps any single
/// directory from filling up with millions of entries.
pub(crate) fn shard(base: &Path, hash: &Hash) -> PathBuf {
    let hex = hash.to_hex();
    base.join(&hex[..2]).join(hex)
}

/// Write bytes to `path` atomically: a reader either sees the old file or the
/// whole new one, never a half-written file. Temp file in the same directory
/// (so the rename stays on one filesystem), then rename over the target.
pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = path.parent().expect("shard path always has a parent");
    std::fs::create_dir_all(dir)?;
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    tmp.write_all(bytes)?;
    tmp.as_file().sync_all()?;
    tmp.persist(path).map_err(|e| e.error)?;
    Ok(())
}
