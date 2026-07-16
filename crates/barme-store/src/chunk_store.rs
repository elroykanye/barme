//! Content-addressed blob store. Chunks are named by their hash, written once,
//! and never mutated. Writing the same bytes twice is a no-op, which is where
//! deduplication actually happens.

use crate::{shard, write_atomic, Result, StoreError};
use barme_core::Hash;
use std::path::{Path, PathBuf};

pub struct ChunkStore {
    root: PathBuf,
}

impl ChunkStore {
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        std::fs::create_dir_all(&root)?;
        Ok(ChunkStore { root })
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
