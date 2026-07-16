//! The engine. Ties chunking, codecs, storage, and GC into the read and write
//! paths, and owns version pointers.
//!
//! Write:  chunk (on original bytes) -> compress each chunk -> dedup/store ->
//!         build manifest -> move pointer
//! Read:   pointer -> manifest -> fetch chunks -> decompress -> verify digest
//!
//! Chunking runs on the original bytes so a local edit only disturbs the
//! chunks it touches. Each chunk is then compressed on its own and addressed
//! by the hash of its compressed form, which keeps the chunk store a pure
//! self-verifying blob store. Two identical original chunks under the same
//! policy compress to the same bytes and still dedup.
//!
//! Both front doors (S3 and native) call this and only this.

use barme_codec::{Codec, CodecError, Raw, Zstd};
use barme_core::{
    Chunking, Fidelity, Hash, Manifest, Original, Quality, Route, Storage, MANIFEST_VERSION,
};
use barme_store::{Store, StoreError};
use sha2::{Digest, Sha256};
use std::path::Path;

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Codec(#[from] CodecError),
    /// A pointer resolved to a manifest that isn't in the store.
    #[error("dangling pointer: manifest {0} is missing")]
    DanglingPointer(Hash),
    /// A manifest referenced a chunk that isn't in the store.
    #[error("missing chunk {0} referenced by manifest {1}")]
    MissingChunk(Hash, Hash),
    /// Reassembled bytes don't match the digest the manifest recorded.
    #[error("integrity: object {0} did not reassemble to its recorded digest")]
    Integrity(Hash),
}

pub type Result<T> = std::result::Result<T, EngineError>;

/// How new objects get written. Per-bucket policy lives on top of this later;
/// for now it's one policy per engine.
#[derive(Debug, Clone)]
pub struct Policy {
    pub codec: String,
    pub zstd_level: i32,
    pub tenant: String,
    /// Recorded verbatim into `manifest.policy_snapshot`.
    pub policy_name: String,
}

impl Default for Policy {
    fn default() -> Self {
        Policy {
            codec: "zstd".into(),
            zstd_level: 0,
            tenant: "default".into(),
            policy_name: "default@v1".into(),
        }
    }
}

pub struct Engine {
    store: Store,
    policy: Policy,
}

impl Engine {
    pub fn open(root: impl AsRef<Path>, policy: Policy) -> Result<Self> {
        Ok(Engine {
            store: Store::open(root)?,
            policy,
        })
    }

    /// Write an object and return its object_id. Prior versions of the same
    /// key stay resolvable; only the pointer moves.
    pub fn put(&self, bucket: &str, key: &str, data: &[u8], content_type: &str) -> Result<Hash> {
        let codec = self.write_codec()?;

        let mut chunks = Vec::new();
        let mut stored_size = 0u64;
        for c in barme_chunk::chunk(data) {
            let encoded = codec.encode(&c.data)?;
            stored_size += encoded.len() as u64;
            chunks.push(self.store.chunks.put(&encoded)?);
        }

        let manifest = Manifest {
            manifest_version: MANIFEST_VERSION,
            object_id: Hash::of(b""), // set by the manifest store
            created_at: now_rfc3339(),
            original: Original {
                size_bytes: data.len() as u64,
                sha256: sha256_hex(data),
                content_type: content_type.to_string(),
            },
            storage: Storage {
                route: Route::Blob,
                fidelity: Fidelity::Exact,
                codec: self.policy.codec.clone(),
                codec_params: self.codec_params(),
                stored_size_bytes: stored_size,
                reconstructs_original: true,
            },
            chunking: Chunking {
                algo: Some("fastcdc".into()),
                chunks,
            },
            quality: Quality::default(),
            tenant: self.policy.tenant.clone(),
            policy_snapshot: self.policy.policy_name.clone(),
        };

        let object_id = self.store.manifests.put(&manifest)?;
        self.store.pointers.set(bucket, key, &object_id)?;
        Ok(object_id)
    }

    /// Read the current version of an object.
    pub fn get(&self, bucket: &str, key: &str) -> Result<Option<Vec<u8>>> {
        let Some(object_id) = self.store.pointers.current(bucket, key)? else {
            return Ok(None);
        };
        Ok(Some(self.read_manifest_bytes(&object_id)?))
    }

    /// Every version of a key, oldest first.
    pub fn history(&self, bucket: &str, key: &str) -> Result<Vec<Hash>> {
        Ok(self.store.pointers.history(bucket, key)?)
    }

    /// The manifest for the current version, without reading the bytes.
    pub fn manifest(&self, bucket: &str, key: &str) -> Result<Option<Manifest>> {
        let Some(object_id) = self.store.pointers.current(bucket, key)? else {
            return Ok(None);
        };
        self.store
            .manifests
            .get(&object_id)?
            .map(Some)
            .ok_or(EngineError::DanglingPointer(object_id))
    }

    /// Read any version directly by object_id, decompress, and verify.
    pub fn read_object(&self, object_id: &Hash) -> Result<Vec<u8>> {
        self.read_manifest_bytes(object_id)
    }

    pub fn delete(&self, bucket: &str, key: &str) -> Result<()> {
        Ok(self.store.pointers.remove(bucket, key)?)
    }

    fn read_manifest_bytes(&self, object_id: &Hash) -> Result<Vec<u8>> {
        let manifest = self
            .store
            .manifests
            .get(object_id)?
            .ok_or(EngineError::DanglingPointer(*object_id))?;
        let codec = barme_codec::decoder_for(&manifest.storage.codec)?;

        let mut out = Vec::with_capacity(manifest.original.size_bytes as usize);
        for h in &manifest.chunking.chunks {
            let stored = self
                .store
                .chunks
                .get(h)?
                .ok_or(EngineError::MissingChunk(*h, *object_id))?;
            out.extend(codec.decode(&stored)?);
        }

        if manifest.storage.fidelity == Fidelity::Exact
            && sha256_hex(&out) != manifest.original.sha256
        {
            return Err(EngineError::Integrity(*object_id));
        }
        Ok(out)
    }

    fn write_codec(&self) -> Result<Box<dyn Codec>> {
        match self.policy.codec.as_str() {
            "none" => Ok(Box::new(Raw)),
            "zstd" => Ok(Box::new(Zstd::new(self.policy.zstd_level))),
            other => Err(CodecError::Unknown(other.to_string()).into()),
        }
    }

    fn codec_params(&self) -> serde_json::Value {
        match self.policy.codec.as_str() {
            "zstd" => serde_json::json!({ "level": self.policy.zstd_level }),
            _ => serde_json::json!({}),
        }
    }
}

fn sha256_hex(data: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(data);
    hex::encode(h.finalize())
}

fn now_rfc3339() -> String {
    use time::format_description::well_known::Rfc3339;
    time::OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_default()
}
