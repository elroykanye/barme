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

    /// Whether this bucket has a persisted config file, i.e. it was explicitly
    /// created or configured rather than merely implied by a first write.
    pub fn exists(&self, bucket: &str) -> bool {
        self.path(bucket).exists()
    }

    /// Every bucket that has a persisted config, decoded from the hex filenames.
    /// Buckets that exist only implicitly (written to, never configured) are not
    /// here; callers union this with the pointer store's bucket list.
    pub fn list(&self) -> Result<Vec<String>> {
        let entries = match std::fs::read_dir(&self.root) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
            Err(e) => return Err(e.into()),
        };
        let mut out = Vec::new();
        for entry in entries {
            let name = entry?.file_name();
            let name = name.to_string_lossy();
            let Some(stem) = name.strip_suffix(".json") else {
                continue;
            };
            let Ok(bytes) = hex::decode(stem) else { continue };
            if let Ok(bucket) = String::from_utf8(bytes) {
                out.push(bucket);
            }
        }
        Ok(out)
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
        s.set_config(
            "photos",
            &BucketConfig {
                public_read: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(s.config("photos").unwrap().public_read);
        // A different bucket is unaffected.
        assert!(!s.config("private").unwrap().public_read);
    }

    #[test]
    fn exists_and_list_track_configured_buckets() {
        let (_d, s) = store();
        assert!(!s.exists("photos"));
        assert!(s.list().unwrap().is_empty());

        s.set_config("photos", &BucketConfig::default()).unwrap();
        s.set_config("videos", &BucketConfig::default()).unwrap();

        assert!(s.exists("photos"));
        assert!(!s.exists("never-made"));
        let mut listed = s.list().unwrap();
        listed.sort();
        assert_eq!(listed, vec!["photos".to_string(), "videos".to_string()]);
    }
}
