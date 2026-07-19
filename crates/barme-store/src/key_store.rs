//! Access keys, one JSON file per key. Small set, read whole when verifying a
//! request, so newly created keys take effect immediately.
//!
//! Secrets are encrypted at rest when the store is opened with a master key (see
//! [`crate::Cipher`]). On disk a record carries either `secret_enc` (encrypted,
//! the normal case) or `secret_key` (legacy plaintext, read for migration). In
//! memory a [`KeyRecord`] always holds the raw secret — the S3 door needs it to
//! verify SigV4.

use crate::{write_atomic, Cipher, Result, StoreError};
use barme_core::KeyRecord;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// On-disk shape of a key record. Exactly one of `secret_enc` / `secret_key`
/// carries the secret; the other is absent. `secret_enc` is the encrypted form,
/// `secret_key` the legacy plaintext kept only so an old store still reads and
/// can be migrated up.
#[derive(Serialize, Deserialize)]
struct StoredKey {
    access_key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    secret_enc: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    secret_key: Option<String>,
    read_only: bool,
    pots: Vec<String>,
    created_at: String,
}

pub struct KeyStore {
    root: PathBuf,
    /// Present when secrets are encrypted at rest. None keeps the legacy
    /// plaintext behaviour (used by tests and unconfigured setups).
    cipher: Option<Cipher>,
}

impl KeyStore {
    /// Open without encryption: secrets are stored as plaintext JSON.
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        Self::open_inner(root, None)
    }

    /// Open with a master key: new secrets are encrypted, and any legacy
    /// plaintext records already on disk are migrated to encrypted form now.
    pub fn open_encrypted(root: impl AsRef<Path>, cipher: Cipher) -> Result<Self> {
        let store = Self::open_inner(root, Some(cipher))?;
        store.migrate_plaintext()?;
        Ok(store)
    }

    fn open_inner(root: impl AsRef<Path>, cipher: Option<Cipher>) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        std::fs::create_dir_all(&root)?;
        Ok(KeyStore { root, cipher })
    }

    pub fn list(&self) -> Result<Vec<KeyRecord>> {
        let entries = match std::fs::read_dir(&self.root) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
            Err(e) => return Err(e.into()),
        };
        let mut out = Vec::new();
        for entry in entries {
            let name = entry?.file_name();
            if name.to_string_lossy().starts_with('.') {
                continue; // temp file from an interrupted write
            }
            let bytes = std::fs::read(self.root.join(&name))?;
            let stored: StoredKey = serde_json::from_slice(&bytes)?;
            out.push(self.decode(stored)?);
        }
        Ok(out)
    }

    pub fn get(&self, access_key: &str) -> Result<Option<KeyRecord>> {
        match std::fs::read(self.path(access_key)) {
            Ok(bytes) => {
                let stored: StoredKey = serde_json::from_slice(&bytes)?;
                Ok(Some(self.decode(stored)?))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn put(&self, record: &KeyRecord) -> Result<()> {
        let stored = self.encode(record)?;
        write_atomic(&self.path(&record.access_key), &serde_json::to_vec(&stored)?)
    }

    pub fn delete(&self, access_key: &str) -> Result<()> {
        match std::fs::remove_file(self.path(access_key)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    /// KeyRecord -> on-disk form, encrypting the secret if a master key is set.
    fn encode(&self, record: &KeyRecord) -> Result<StoredKey> {
        let (secret_enc, secret_key) = match &self.cipher {
            Some(c) => (Some(c.encrypt(&record.secret_key)?), None),
            None => (None, Some(record.secret_key.clone())),
        };
        Ok(StoredKey {
            access_key: record.access_key.clone(),
            secret_enc,
            secret_key,
            read_only: record.read_only,
            pots: record.pots.clone(),
            created_at: record.created_at.clone(),
        })
    }

    /// On-disk form -> KeyRecord, decrypting `secret_enc` or reading legacy
    /// `secret_key`. An encrypted record with no master key configured is an
    /// error, not a silent miss.
    fn decode(&self, stored: StoredKey) -> Result<KeyRecord> {
        let secret_key = match (stored.secret_enc, stored.secret_key) {
            (Some(enc), _) => match &self.cipher {
                Some(c) => c.decrypt(&enc)?,
                None => {
                    return Err(StoreError::Crypto(
                        "key store is encrypted but no master key is configured".into(),
                    ))
                }
            },
            (None, Some(plain)) => plain, // legacy plaintext
            (None, None) => return Err(StoreError::Crypto("key record has no secret".into())),
        };
        Ok(KeyRecord {
            access_key: stored.access_key,
            secret_key,
            read_only: stored.read_only,
            pots: stored.pots,
            created_at: stored.created_at,
        })
    }

    /// Re-encrypt any legacy plaintext records. Called once on encrypted open so
    /// an existing plaintext store upgrades itself in place. Idempotent: records
    /// already encrypted are left alone.
    fn migrate_plaintext(&self) -> Result<()> {
        let entries = match std::fs::read_dir(&self.root) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e.into()),
        };
        for entry in entries {
            let name = entry?.file_name();
            if name.to_string_lossy().starts_with('.') {
                continue;
            }
            let stored: StoredKey = serde_json::from_slice(&std::fs::read(self.root.join(&name))?)?;
            if stored.secret_enc.is_none() && stored.secret_key.is_some() {
                // decode reads the plaintext; put re-writes it encrypted.
                let record = self.decode(stored)?;
                self.put(&record)?;
            }
        }
        Ok(())
    }

    fn path(&self, access_key: &str) -> PathBuf {
        self.root
            .join(format!("{}.json", hex::encode(access_key.as_bytes())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(access: &str, secret: &str) -> KeyRecord {
        KeyRecord {
            access_key: access.into(),
            secret_key: secret.into(),
            read_only: false,
            pots: vec![],
            created_at: "2026-07-19T00:00:00Z".into(),
        }
    }

    #[test]
    fn plaintext_round_trips_without_cipher() {
        let dir = tempfile::tempdir().unwrap();
        let s = KeyStore::open(dir.path()).unwrap();
        s.put(&record("ak", "sk")).unwrap();
        assert_eq!(s.get("ak").unwrap().unwrap().secret_key, "sk");
    }

    #[test]
    fn encrypted_round_trips_and_no_plaintext_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let s = KeyStore::open_encrypted(dir.path(), Cipher::new(&[9u8; 32])).unwrap();
        s.put(&record("ak", "top-secret-value")).unwrap();

        // The raw file must not contain the secret in the clear.
        let raw = std::fs::read_to_string(s.path("ak")).unwrap();
        assert!(!raw.contains("top-secret-value"), "secret leaked to disk: {raw}");
        assert!(raw.contains("secret_enc"));

        // But a reader with the same master key recovers it.
        assert_eq!(s.get("ak").unwrap().unwrap().secret_key, "top-secret-value");
    }

    #[test]
    fn wrong_master_key_cannot_read() {
        let dir = tempfile::tempdir().unwrap();
        KeyStore::open_encrypted(dir.path(), Cipher::new(&[1u8; 32]))
            .unwrap()
            .put(&record("ak", "sk"))
            .unwrap();
        // Reopen with a different master key: the secret must not decrypt.
        let other = KeyStore::open_encrypted(dir.path(), Cipher::new(&[2u8; 32]));
        // migrate_plaintext runs on open but these are encrypted (skipped), so
        // open succeeds; the failure surfaces on read.
        let store = other.unwrap();
        assert!(matches!(store.get("ak"), Err(StoreError::Crypto(_))));
    }

    #[test]
    fn legacy_plaintext_is_migrated_on_encrypted_open() {
        let dir = tempfile::tempdir().unwrap();
        // Seed a plaintext record the old way.
        KeyStore::open(dir.path())
            .unwrap()
            .put(&record("ak", "old-plain-secret"))
            .unwrap();
        let path = dir.path().join(format!("{}.json", hex::encode(b"ak")));
        assert!(std::fs::read_to_string(&path).unwrap().contains("old-plain-secret"));

        // Open encrypted: migration rewrites it as ciphertext, still readable.
        let s = KeyStore::open_encrypted(dir.path(), Cipher::new(&[5u8; 32])).unwrap();
        assert_eq!(s.get("ak").unwrap().unwrap().secret_key, "old-plain-secret");
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(!raw.contains("old-plain-secret"), "migration left plaintext: {raw}");
        assert!(raw.contains("secret_enc"));
    }
}
