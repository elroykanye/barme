//! Immutable manifest store, content-addressed like chunks.
//!
//! A manifest is addressed by the hash of its own content, but it also carries
//! that address in its `object_id` field. To avoid the self-reference, the id
//! is computed over the manifest with `object_id` excluded, then written back
//! in. A read recomputes the same way and checks it matches, so a tampered
//! manifest is caught exactly like a tampered chunk.

use crate::{shard, write_atomic, Result, StoreError};
use barme_core::{Hash, Manifest, MANIFEST_VERSION};
use std::path::{Path, PathBuf};

pub struct ManifestStore {
    root: PathBuf,
}

impl ManifestStore {
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        std::fs::create_dir_all(&root)?;
        Ok(ManifestStore { root })
    }

    /// Store a manifest and return its object_id. The passed manifest's own
    /// `object_id` is ignored on the way in and set to the computed id.
    pub fn put(&self, manifest: &Manifest) -> Result<Hash> {
        let id = manifest_id(manifest)?;
        let mut stored = manifest.clone();
        stored.object_id = id;

        let path = shard(&self.root, &id);
        if !path.exists() {
            write_atomic(&path, &serde_json::to_vec(&stored)?)?;
        }
        Ok(id)
    }

    pub fn get(&self, id: &Hash) -> Result<Option<Manifest>> {
        let path = shard(&self.root, id);
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let manifest: Manifest = serde_json::from_slice(&bytes)?;
        // Refuse a manifest from a newer barme before trusting its fields: a
        // version past what we know may mean a field changed meaning. Checked
        // before the integrity match so the message is about the version, not a
        // hash mismatch.
        if manifest.manifest_version > MANIFEST_VERSION {
            return Err(StoreError::UnsupportedManifest {
                found: manifest.manifest_version,
                supported: MANIFEST_VERSION,
            });
        }
        if manifest_id(&manifest)? != *id {
            return Err(StoreError::Integrity {
                addr: id.to_string(),
            });
        }
        Ok(Some(manifest))
    }

    pub fn has(&self, id: &Hash) -> bool {
        shard(&self.root, id).exists()
    }
}

/// The object_id: hash of the manifest's canonical JSON with `object_id`
/// removed. serde_json sorts map keys, so the encoding is stable regardless of
/// struct field order.
fn manifest_id(manifest: &Manifest) -> Result<Hash> {
    let mut value = serde_json::to_value(manifest)?;
    if let Some(obj) = value.as_object_mut() {
        obj.remove("object_id");
    }
    Ok(Hash::of(&serde_json::to_vec(&value)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use barme_core::{Chunking, Fidelity, Original, Quality, Route, Storage, MANIFEST_VERSION};

    fn sample() -> Manifest {
        Manifest {
            manifest_version: MANIFEST_VERSION,
            object_id: Hash::of(b"placeholder"),
            created_at: "2026-07-16T10:22:04Z".into(),
            original: Original {
                size_bytes: 12,
                sha256: "e3b0c4".into(),
                content_type: "text/plain".into(),
            },
            storage: Storage {
                route: Route::Blob,
                fidelity: Fidelity::Exact,
                codec: "zstd".into(),
                codec_params: serde_json::json!({ "level": 3 }),
                stored_size_bytes: 8,
                reconstructs_original: true,
            },
            chunking: Chunking {
                algo: Some("fastcdc".into()),
                chunks: vec![Hash::of(b"c1"), Hash::of(b"c2")],
                merkle_root: None,
            },
            quality: Quality::default(),
            tenant: "acme".into(),
            policy_snapshot: "default@v1".into(),
        }
    }

    fn store() -> (tempfile::TempDir, ManifestStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = ManifestStore::open(dir.path()).unwrap();
        (dir, store)
    }

    #[test]
    fn put_sets_object_id_and_round_trips() {
        let (_d, s) = store();
        let id = s.put(&sample()).unwrap();
        let got = s.get(&id).unwrap().unwrap();
        // The stored manifest carries the real id, not the placeholder.
        assert_eq!(got.object_id, id);
        assert_eq!(got.storage.codec, "zstd");
    }

    #[test]
    fn id_is_independent_of_incoming_object_id() {
        let (_d, s) = store();
        let mut a = sample();
        let mut b = sample();
        a.object_id = Hash::of(b"one");
        b.object_id = Hash::of(b"two");
        // Same content apart from object_id -> same address.
        assert_eq!(s.put(&a).unwrap(), s.put(&b).unwrap());
    }

    #[test]
    fn tampered_manifest_is_caught() {
        let (_d, s) = store();
        let id = s.put(&sample()).unwrap();
        let mut tampered = sample();
        tampered.object_id = id; // keep the claimed id...
        tampered.tenant = "attacker".into(); // ...but change the content
        std::fs::write(shard(&s.root, &id), serde_json::to_vec(&tampered).unwrap()).unwrap();
        assert!(matches!(s.get(&id), Err(StoreError::Integrity { .. })));
    }

    #[test]
    fn manifest_from_a_newer_barme_is_refused() {
        let (_d, s) = store();
        // A manifest stamped with a version this build doesn't know. put()
        // addresses it by its own (v2-inclusive) content, so the id matches on
        // read; the version guard must reject it before that.
        let mut future = sample();
        future.manifest_version = MANIFEST_VERSION + 1;
        let id = s.put(&future).unwrap();
        assert!(matches!(
            s.get(&id),
            Err(StoreError::UnsupportedManifest { found, supported })
                if found == MANIFEST_VERSION + 1 && supported == MANIFEST_VERSION
        ));
    }
}
