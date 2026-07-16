//! Reverse index: `object_id -> [(pot, key), ...]`. The content address alone
//! doesn't say where an object is shelved, and semantic search returns bare
//! object ids; this store lets a hit be resolved back to the pots and keys that
//! point at it, so search results can name a location.
//!
//! Content-addressed like manifests (git-style shard dirs), but the value is a
//! plain list appended to, not immutable.

use crate::{shard, write_atomic, Result};
use barme_core::Hash;
use std::path::{Path, PathBuf};

pub struct ReverseStore {
    root: PathBuf,
}

impl ReverseStore {
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        std::fs::create_dir_all(&root)?;
        Ok(ReverseStore { root })
    }

    /// Locations pointing at an object, in insertion order. Empty if unknown.
    pub fn get(&self, id: &Hash) -> Result<Vec<(String, String)>> {
        match std::fs::read(shard(&self.root, id)) {
            Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(vec![]),
            Err(e) => Err(e.into()),
        }
    }

    /// Record that `pot/key` points at `id`. Idempotent: a location already
    /// present isn't added twice.
    pub fn add(&self, id: &Hash, pot: &str, key: &str) -> Result<()> {
        let mut locations = self.get(id)?;
        let loc = (pot.to_string(), key.to_string());
        if !locations.contains(&loc) {
            locations.push(loc);
            write_atomic(&shard(&self.root, id), &serde_json::to_vec(&locations)?)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, ReverseStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = ReverseStore::open(dir.path()).unwrap();
        (dir, store)
    }

    #[test]
    fn unknown_id_is_empty() {
        let (_d, s) = store();
        assert!(s.get(&Hash::of(b"ghost")).unwrap().is_empty());
    }

    #[test]
    fn add_accumulates_and_dedups() {
        let (_d, s) = store();
        let id = Hash::of(b"obj");
        s.add(&id, "a", "k1").unwrap();
        s.add(&id, "b", "k2").unwrap();
        s.add(&id, "a", "k1").unwrap(); // duplicate ignored
        assert_eq!(
            s.get(&id).unwrap(),
            vec![("a".into(), "k1".into()), ("b".into(), "k2".into())]
        );
    }
}
