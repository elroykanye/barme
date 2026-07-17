//! Content-addressed blob store. Chunks are named by their hash, written once,
//! and never mutated. Writing the same bytes twice is a no-op, which is where
//! deduplication actually happens.

use crate::{shard, write_atomic, Result, StoreError};
use barme_core::Hash;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Name of the condemned-set file, sitting at the chunk root beside the shard
/// dirs. Leading dot keeps it out of `all()`, which only descends the two-hex
/// shard directories.
const CONDEMNED: &str = ".condemned";

pub struct ChunkStore {
    root: PathBuf,
    /// Chunks written by an upload still in progress: on disk but not yet
    /// referenced by any pointer, so `mark` can't see them. Held in memory only.
    /// GC treats these as reachable, so a sweep can never erase a chunk out from
    /// under a live upload no matter how tight the grace window is. A crash drops
    /// the set — correct, since a crashed upload's orphan chunks *are* garbage and
    /// should be collected normally.
    pinned: Arc<Mutex<HashSet<Hash>>>,
}

impl ChunkStore {
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        std::fs::create_dir_all(&root)?;
        Ok(ChunkStore {
            root,
            pinned: Arc::new(Mutex::new(HashSet::new())),
        })
    }

    /// Pin a chunk as in-flight so GC won't reclaim it before its object commits.
    /// Call right after `put`; pair with `unpin` once the pointer is set.
    pub fn pin(&self, hash: &Hash) {
        self.pinned.lock().unwrap().insert(*hash);
    }

    /// Release in-flight pins once the object is committed (its chunks are now
    /// reachable through the pointer) or the upload was abandoned.
    pub fn unpin(&self, hashes: &[Hash]) {
        let mut set = self.pinned.lock().unwrap();
        for h in hashes {
            set.remove(h);
        }
    }

    /// Snapshot of currently pinned chunks. GC unions this into its reachable set.
    pub fn pinned(&self) -> HashSet<Hash> {
        self.pinned.lock().unwrap().clone()
    }

    /// Store bytes and return their address. If an identical chunk is already
    /// present, nothing is written; the hash is enough to know they match.
    pub fn put(&self, data: &[u8]) -> Result<Hash> {
        let hash = Hash::of(data);
        let path = shard(&self.root, &hash);
        if !path.exists() {
            write_atomic(&path, data)?;
        }
        Ok(hash)
    }

    /// Fetch a chunk, verifying its bytes hash back to the address they were
    /// requested by. A flipped byte on disk surfaces here as an error.
    pub fn get(&self, hash: &Hash) -> Result<Option<Vec<u8>>> {
        let path = shard(&self.root, hash);
        let data = match std::fs::read(&path) {
            Ok(d) => d,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        if Hash::of(&data) != *hash {
            return Err(StoreError::Integrity {
                addr: hash.to_string(),
            });
        }
        Ok(Some(data))
    }

    pub fn has(&self, hash: &Hash) -> bool {
        shard(&self.root, hash).exists()
    }

    /// Remove a chunk. Not called on delete; only GC erases chunks, after the
    /// grace period. Absent chunk is not an error.
    pub fn delete(&self, hash: &Hash) -> Result<()> {
        match std::fs::remove_file(shard(&self.root, hash)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    /// Every chunk currently stored. GC's sweep walks this against the reachable
    /// set. Descends the two-hex shard dirs and parses filenames back to hashes.
    pub fn all(&self) -> Result<Vec<Hash>> {
        let shards = match std::fs::read_dir(&self.root) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
            Err(e) => return Err(e.into()),
        };
        let mut out = Vec::new();
        for shard in shards {
            let shard = shard?;
            if !shard.file_type()?.is_dir() {
                continue; // skips the .condemned file
            }
            for entry in std::fs::read_dir(shard.path())? {
                let name = entry?.file_name();
                let hex = name.to_string_lossy();
                // Skip anything that isn't a chunk file: a temp file from an
                // in-flight or crashed write, or any stray dotfile. Chunk names
                // are bare hex, so a leading dot is enough to tell them apart.
                if hex.starts_with('.') {
                    continue;
                }
                out.push(format!("blake3:{hex}").parse()?);
            }
        }
        Ok(out)
    }

    /// Total bytes of all stored chunks on disk: the real, deduplicated,
    /// compressed footprint. Walks the shard dirs summing file sizes.
    pub fn physical_bytes(&self) -> Result<u64> {
        let shards = match std::fs::read_dir(&self.root) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(e) => return Err(e.into()),
        };
        let mut total = 0u64;
        for shard in shards {
            let shard = shard?;
            if !shard.file_type()?.is_dir() {
                continue;
            }
            for entry in std::fs::read_dir(shard.path())? {
                let entry = entry?;
                if entry.file_name().to_string_lossy().starts_with('.') {
                    continue; // temp/stray file, not a chunk
                }
                total += entry.metadata()?.len();
            }
        }
        Ok(total)
    }

    /// How many unique chunks are stored.
    pub fn count(&self) -> Result<usize> {
        Ok(self.all()?.len())
    }

    /// The condemned set: chunk -> unix-seconds it was first condemned. GC reads
    /// it whole, mutates in memory, and writes it back with `save_condemned`.
    ///
    /// A corrupt file heals to empty instead of erroring. The set is disposable
    /// derived state — mark-and-sweep re-derives reachability every pass, and the
    /// stamps only gate the grace window — so losing it just re-condemns chunks
    /// with fresh timestamps, delaying their collection by at most one grace
    /// period. Propagating the parse error instead would wedge GC forever on a
    /// single bad byte and let the disk fill without bound; self-healing is both
    /// safe and in keeping with the collector re-deriving its own truth.
    pub fn load_condemned(&self) -> Result<HashMap<Hash, u64>> {
        match std::fs::read(self.root.join(CONDEMNED)) {
            Ok(bytes) => Ok(serde_json::from_slice(&bytes).unwrap_or_default()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(HashMap::new()),
            Err(e) => Err(e.into()),
        }
    }

    /// Replace the condemned set atomically.
    pub fn save_condemned(&self, set: &HashMap<Hash, u64>) -> Result<()> {
        write_atomic(&self.root.join(CONDEMNED), &serde_json::to_vec(set)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, ChunkStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = ChunkStore::open(dir.path()).unwrap();
        (dir, store)
    }

    #[test]
    fn put_then_get_round_trips() {
        let (_d, s) = store();
        let h = s.put(b"holiday footage").unwrap();
        assert_eq!(s.get(&h).unwrap().unwrap(), b"holiday footage");
    }

    #[test]
    fn missing_chunk_is_none() {
        let (_d, s) = store();
        let h = Hash::of(b"never stored");
        assert!(s.get(&h).unwrap().is_none());
        assert!(!s.has(&h));
    }

    #[test]
    fn identical_bytes_dedup_to_one_address() {
        let (_d, s) = store();
        let a = s.put(b"same bytes").unwrap();
        let b = s.put(b"same bytes").unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn corruption_is_caught_on_read() {
        let (_d, s) = store();
        let h = s.put(b"trust but verify").unwrap();
        // Tamper with the file behind the store's back.
        std::fs::write(shard(&s.root, &h), b"tampered content!").unwrap();
        assert!(matches!(s.get(&h), Err(StoreError::Integrity { .. })));
    }
}
