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

mod annotation_store;
mod chunk_store;
mod key_store;
mod manifest_store;
mod meta_store;
mod pointer_store;
mod reverse_store;
mod webhook_store;

pub use annotation_store::AnnotationStore;
pub use chunk_store::ChunkStore;
pub use key_store::KeyStore;
pub use manifest_store::ManifestStore;
pub use meta_store::MetaStore;
pub use pointer_store::PointerStore;
pub use reverse_store::ReverseStore;
pub use webhook_store::WebhookStore;

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
    pub meta: MetaStore,
    pub keys: KeyStore,
    pub annotations: AnnotationStore,
    pub reverse: ReverseStore,
    pub webhooks: WebhookStore,
    /// Temp files left by a crashed write and reaped on this open. Zero on a
    /// clean start; non-zero means the last run was killed mid-write. The
    /// daemon logs it.
    pub recovered_temp: usize,
}

impl Store {
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref();
        // Reap any temp files a previous crash left behind before anything walks
        // the shard dirs, so a half-written file can't trip an enumerator.
        let recovered_temp = sweep_temp(root)?;
        Ok(Store {
            chunks: ChunkStore::open(root.join("chunks"))?,
            manifests: ManifestStore::open(root.join("manifests"))?,
            pointers: PointerStore::open(root.join("pointers"))?,
            meta: MetaStore::open(root.join("meta"))?,
            keys: KeyStore::open(root.join("keys"))?,
            annotations: AnnotationStore::open(root.join("annotations"))?,
            reverse: ReverseStore::open(root.join("reverse"))?,
            webhooks: WebhookStore::open(root.join("webhooks"))?,
            recovered_temp,
        })
    }
}

/// Git-style sharded path: `base/<first 2 hex>/<full hex>`. Keeps any single
/// directory from filling up with millions of entries.
pub(crate) fn shard(base: &Path, hash: &Hash) -> PathBuf {
    let hex = hash.to_hex();
    base.join(&hex[..2]).join(hex)
}

/// Prefix for the temp file used by `write_atomic`. A crash between create and
/// rename leaves one of these behind; the prefix lets `sweep_temp` find them and
/// the shard walkers skip them (chunk/manifest names are hex, never dotted).
pub(crate) const TMP_PREFIX: &str = ".barme-tmp-";

/// Write bytes to `path` atomically *and durably*: a reader either sees the old
/// file or the whole new one, and once this returns the new file survives power
/// loss. Temp file in the same directory (so the rename stays on one
/// filesystem), fsync its contents, rename over the target, then fsync the
/// directory so the rename itself is durable — a synced file whose rename was
/// lost is still lost.
pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = path.parent().expect("shard path always has a parent");
    // Whether the containing dir is new decides if the grandparent needs a sync
    // too (so the new dir entry itself is durable, not just the file in it).
    let dir_is_new = !dir.exists();
    std::fs::create_dir_all(dir)?;
    let mut tmp = tempfile::Builder::new().prefix(TMP_PREFIX).tempfile_in(dir)?;
    tmp.write_all(bytes)?;
    tmp.as_file().sync_all()?;
    tmp.persist(path).map_err(|e| e.error)?;
    sync_dir(dir)?;
    if dir_is_new {
        if let Some(parent) = dir.parent() {
            sync_dir(parent)?;
        }
    }
    Ok(())
}

/// fsync a directory so a create/rename into it is durable. The bytes were
/// already synced; this persists the *name*. On Unix that's an fsync on the
/// directory fd. On Windows a directory can't be opened as a file and NTFS
/// journals its own metadata, so it's a no-op there.
#[cfg(unix)]
pub(crate) fn sync_dir(dir: &Path) -> Result<()> {
    std::fs::File::open(dir)?.sync_all()?;
    Ok(())
}

#[cfg(not(unix))]
pub(crate) fn sync_dir(_dir: &Path) -> Result<()> {
    Ok(())
}

/// Reap temp files a crashed `write_atomic` left behind, walking the whole data
/// root. NamedTempFile deletes itself on Drop, but a hard kill skips Drop and
/// strands the file. Run once on open, before any shard dir is enumerated.
/// Returns how many were removed.
pub(crate) fn sweep_temp(root: &Path) -> Result<usize> {
    fn walk(dir: &Path, n: &mut usize) -> Result<()> {
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e.into()),
        };
        for entry in entries {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                walk(&entry.path(), n)?;
            } else if entry
                .file_name()
                .to_string_lossy()
                .starts_with(TMP_PREFIX)
            {
                std::fs::remove_file(entry.path())?;
                *n += 1;
            }
        }
        Ok(())
    }
    let mut n = 0;
    walk(root, &mut n)?;
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Simulate a crash mid-write: a real chunk plus a stranded temp file in the
    /// same shard dir. Recovery reaps the temp file, the enumerators ignore it,
    /// and the real chunk survives untouched.
    #[test]
    fn crash_leftover_temp_is_reaped_and_never_counted() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        // A real chunk, written durably.
        let store = Store::open(root).unwrap();
        let h = store.chunks.put(b"survivor bytes").unwrap();
        assert_eq!(store.recovered_temp, 0);

        // Hand-plant a temp file next to it, as a kill -9 between create and
        // rename would leave. It sits in the same two-hex shard dir.
        let shard_dir = shard(&root.join("chunks"), &h).parent().unwrap().to_path_buf();
        let stray = shard_dir.join(format!("{TMP_PREFIX}abc123"));
        std::fs::write(&stray, b"half written").unwrap();

        // Even before recovery, the walkers must not choke on it or count it.
        assert_eq!(store.chunks.count().unwrap(), 1);
        assert_eq!(store.chunks.all().unwrap(), vec![h]);

        // Reopening reaps the stray and reports it.
        let reopened = Store::open(root).unwrap();
        assert_eq!(reopened.recovered_temp, 1);
        assert!(!stray.exists());
        assert_eq!(reopened.chunks.get(&h).unwrap().unwrap(), b"survivor bytes");

        // A clean reopen finds nothing to recover.
        let clean = Store::open(root).unwrap();
        assert_eq!(clean.recovered_temp, 0);
    }

    /// write_atomic must leave no temp file behind on the happy path (Drop/persist
    /// cleans up), so a clean run always recovers zero.
    #[test]
    fn clean_writes_leave_no_temp_files() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        for i in 0..50u32 {
            store.chunks.put(format!("chunk {i}").as_bytes()).unwrap();
        }
        assert_eq!(sweep_temp(dir.path()).unwrap(), 0);
    }
}
