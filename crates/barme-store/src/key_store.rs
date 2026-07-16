//! Access keys, one JSON file per key. Small set, read whole when verifying a
//! request, so newly created keys take effect immediately.

use crate::{write_atomic, Result};
use barme_core::KeyRecord;
use std::path::{Path, PathBuf};

pub struct KeyStore {
    root: PathBuf,
}

impl KeyStore {
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        std::fs::create_dir_all(&root)?;
        Ok(KeyStore { root })
    }

    pub fn list(&self) -> Result<Vec<KeyRecord>> {
        let entries = match std::fs::read_dir(&self.root) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
            Err(e) => return Err(e.into()),
        };
        let mut out = Vec::new();
        for entry in entries {
            let bytes = std::fs::read(entry?.path())?;
            out.push(serde_json::from_slice(&bytes)?);
        }
        Ok(out)
    }

    pub fn get(&self, access_key: &str) -> Result<Option<KeyRecord>> {
        match std::fs::read(self.path(access_key)) {
            Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn put(&self, record: &KeyRecord) -> Result<()> {
        write_atomic(&self.path(&record.access_key), &serde_json::to_vec(record)?)
    }

    pub fn delete(&self, access_key: &str) -> Result<()> {
        match std::fs::remove_file(self.path(access_key)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    fn path(&self, access_key: &str) -> PathBuf {
        self.root
            .join(format!("{}.json", hex::encode(access_key.as_bytes())))
    }
}
