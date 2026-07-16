//! Pointers: `bucket/key -> manifest hash`. The only mutable state in the
//! store, and the reason updates and rollbacks are cheap.
//!
//! Each pointer is an append-only list of manifest hashes, one per line,
//! oldest first. The last line is the current version. A new version appends;
//! a rollback appends an older hash again. The whole file is rewritten
//! atomically on every change, so a reader never sees a torn update.
//!
//! Keys are hex-encoded into a single filename, so arbitrary S3 keys (slashes,
//! unicode, whatever) can't escape the bucket directory or collide.

use crate::{write_atomic, Result, StoreError};
use barme_core::Hash;
use std::path::{Path, PathBuf};

pub struct PointerStore {
    root: PathBuf,
}

impl PointerStore {
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        std::fs::create_dir_all(&root)?;
        Ok(PointerStore { root })
    }

    /// Point `bucket/key` at a manifest, keeping prior versions in history.
    pub fn set(&self, bucket: &str, key: &str, manifest: &Hash) -> Result<()> {
        let path = self.path(bucket, key)?;
        let mut contents = read_to_string_opt(&path)?.unwrap_or_default();
        contents.push_str(&manifest.to_string());
        contents.push('\n');
        write_atomic(&path, contents.as_bytes())
    }

    /// The current version, or None if the key was never set or was removed.
    pub fn current(&self, bucket: &str, key: &str) -> Result<Option<Hash>> {
        Ok(self.history(bucket, key)?.pop())
    }

    /// Every version this key has pointed at, oldest first.
    pub fn history(&self, bucket: &str, key: &str) -> Result<Vec<Hash>> {
        let path = self.path(bucket, key)?;
        let Some(contents) = read_to_string_opt(&path)? else {
            return Ok(vec![]);
        };
        contents
            .lines()
            .filter(|l| !l.is_empty())
            .map(|l| l.parse::<Hash>().map_err(Into::into))
            .collect()
    }

    /// Keys currently present in a bucket. Order is unspecified.
    pub fn list(&self, bucket: &str) -> Result<Vec<String>> {
        let dir = self.bucket_dir(bucket)?;
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
            Err(e) => return Err(e.into()),
        };
        let mut keys = Vec::new();
        for entry in entries {
            let name = entry?.file_name();
            let bytes = hex::decode(name.to_string_lossy().as_bytes())
                .map_err(|_| StoreError::BadBucket(bucket.to_string()))?;
            keys.push(String::from_utf8_lossy(&bytes).into_owned());
        }
        Ok(keys)
    }

    /// Drop a pointer and its history. (A versioned delete-marker scheme can
    /// come later; this is the plain remove.)
    pub fn remove(&self, bucket: &str, key: &str) -> Result<()> {
        match std::fs::remove_file(self.path(bucket, key)?) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    fn bucket_dir(&self, bucket: &str) -> Result<PathBuf> {
        if bucket.is_empty()
            || bucket == "."
            || bucket == ".."
            || bucket.contains('/')
            || bucket.contains('\\')
        {
            return Err(StoreError::BadBucket(bucket.to_string()));
        }
        Ok(self.root.join(bucket))
    }

    fn path(&self, bucket: &str, key: &str) -> Result<PathBuf> {
        Ok(self.bucket_dir(bucket)?.join(hex::encode(key.as_bytes())))
    }
}

fn read_to_string_opt(path: &Path) -> Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(s) => Ok(Some(s)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, PointerStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = PointerStore::open(dir.path()).unwrap();
        (dir, store)
    }

    #[test]
    fn set_then_current() {
        let (_d, s) = store();
        let m = Hash::of(b"m1");
        s.set("photos", "cat.jpg", &m).unwrap();
        assert_eq!(s.current("photos", "cat.jpg").unwrap(), Some(m));
    }

    #[test]
    fn unset_key_is_none() {
        let (_d, s) = store();
        assert_eq!(s.current("photos", "ghost.jpg").unwrap(), None);
        assert!(s.history("photos", "ghost.jpg").unwrap().is_empty());
    }

    #[test]
    fn history_accumulates_oldest_first() {
        let (_d, s) = store();
        let (v1, v2) = (Hash::of(b"v1"), Hash::of(b"v2"));
        s.set("b", "k", &v1).unwrap();
        s.set("b", "k", &v2).unwrap();
        assert_eq!(s.history("b", "k").unwrap(), vec![v1, v2]);
        assert_eq!(s.current("b", "k").unwrap(), Some(v2));
    }

    #[test]
    fn rollback_is_an_append() {
        let (_d, s) = store();
        let (v1, v2) = (Hash::of(b"v1"), Hash::of(b"v2"));
        s.set("b", "k", &v1).unwrap();
        s.set("b", "k", &v2).unwrap();
        s.set("b", "k", &v1).unwrap(); // roll back to v1
        assert_eq!(s.current("b", "k").unwrap(), Some(v1));
        assert_eq!(s.history("b", "k").unwrap(), vec![v1, v2, v1]);
    }

    #[test]
    fn list_returns_keys_including_slashed_ones() {
        let (_d, s) = store();
        s.set("b", "a/b/c.txt", &Hash::of(b"x")).unwrap();
        s.set("b", "top.txt", &Hash::of(b"y")).unwrap();
        let mut keys = s.list("b").unwrap();
        keys.sort();
        assert_eq!(keys, vec!["a/b/c.txt".to_string(), "top.txt".to_string()]);
    }

    #[test]
    fn remove_clears_the_pointer() {
        let (_d, s) = store();
        s.set("b", "k", &Hash::of(b"x")).unwrap();
        s.remove("b", "k").unwrap();
        assert_eq!(s.current("b", "k").unwrap(), None);
    }

    #[test]
    fn bad_bucket_is_rejected() {
        let (_d, s) = store();
        let h = Hash::of(b"x");
        assert!(matches!(
            s.set("../escape", "k", &h),
            Err(StoreError::BadBucket(_))
        ));
        assert!(matches!(s.set("", "k", &h), Err(StoreError::BadBucket(_))));
    }
}
