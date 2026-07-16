//! Per-bucket configuration, stored as one small JSON file per bucket. A bucket
//! with no file yet reads back as the default config (private).

use crate::{write_atomic, Result};
use barme_core::BucketConfig;
use std::path::{Path, PathBuf};

pub struct MetaStore {
    root: PathBuf,
}

impl MetaStore {
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        std::fs::create_dir_all(&root)?;
        Ok(MetaStore { root })
    }

    pub fn config(&self, bucket: &str) -> Result<BucketConfig> {
        match std::fs::read(self.path(bucket)) {
            Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(BucketConfig::default()),
            Err(e) => Err(e.into()),
        }
    }

    pub fn set_config(&self, bucket: &str, config: &BucketConfig) -> Result<()> {
        write_atomic(&self.path(bucket), &serde_json::to_vec(config)?)
    }

    /// Carry a bucket's config over to a new name.
    pub fn rename_bucket(&self, old: &str, new: &str) -> Result<()> {
        let from = self.path(old);
        if from.exists() {
            std::fs::rename(from, self.path(new))?;
        }
        Ok(())
    }

    /// Forget a bucket's config.
    pub fn delete_bucket(&self, bucket: &str) -> Result<()> {
        match std::fs::remove_file(self.path(bucket)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    // Hex-encode the bucket name into a single filename, same trick the pointer
    // store uses for keys, so odd bucket names can't escape the directory.
    fn path(&self, bucket: &str) -> PathBuf {
        self.root.join(format!("{}.json", hex::encode(bucket.as_bytes())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, MetaStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = MetaStore::open(dir.path()).unwrap();
        (dir, store)
    }

    #[test]
    fn unset_bucket_is_private_by_default() {
        let (_d, s) = store();
        assert!(!s.config("photos").unwrap().public_read);
    }

    #[test]
    fn set_then_read_back() {
        let (_d, s) = store();
        s.set_config("photos", &BucketConfig { public_read: true }).unwrap();
        assert!(s.config("photos").unwrap().public_read);
        // A different bucket is unaffected.
        assert!(!s.config("private").unwrap().public_read);
    }
}
