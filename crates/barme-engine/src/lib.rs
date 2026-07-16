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
use std::sync::Arc;

/// Emitted after a successful write, for anything that wants to react to new
/// objects (the semantic layer, mostly). Handed to the write hook by value.
pub struct WriteEvent {
    pub object_id: Hash,
    pub tenant: String,
    pub content_type: String,
    pub bytes: Vec<u8>,
}

type WriteHook = Arc<dyn Fn(WriteEvent) + Send + Sync>;

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

/// Storage-wide statistics. See [`Engine::stats`].
#[derive(Debug, Clone, serde::Serialize)]
pub struct Stats {
    pub buckets: usize,
    pub objects: usize,
    pub logical_bytes: u64,
    pub physical_bytes: u64,
    pub unique_chunks: usize,
}

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
    write_hook: Option<WriteHook>,
}

impl Engine {
    pub fn open(root: impl AsRef<Path>, policy: Policy) -> Result<Self> {
        Ok(Engine {
            store: Store::open(root)?,
            policy,
            write_hook: None,
        })
    }

    /// Register a hook to run after every successful write. Set it before the
    /// engine is shared. The hook must be cheap and non-blocking; the intended
    /// use is to drop the event on a queue for a background worker.
    pub fn set_write_hook<F>(&mut self, hook: F)
    where
        F: Fn(WriteEvent) + Send + Sync + 'static,
    {
        self.write_hook = Some(Arc::new(hook));
    }

    /// Write an object and return its object_id. Prior versions of the same
    /// key stay resolvable; only the pointer moves.
    pub fn put(&self, bucket: &str, key: &str, data: &[u8], content_type: &str) -> Result<Hash> {
        // Effective policy: the pot's overrides, falling back to the server
        // default. This is where per-pot config actually takes effect.
        let cfg = self.store.meta.config(bucket)?;
        let codec_name = cfg.codec.clone().unwrap_or_else(|| self.policy.codec.clone());
        let level = cfg.zstd_level.unwrap_or(self.policy.zstd_level);
        let fidelity = match cfg.fidelity.as_deref() {
            Some("perceptual") => Fidelity::Perceptual,
            _ => Fidelity::Exact,
        };
        let route = if cfg.route_by_content_type && content_type.starts_with("image/") {
            Route::Image
        } else {
            Route::Blob
        };

        let codec = build_codec(&codec_name, level)?;

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
                route,
                fidelity,
                codec: codec_name.clone(),
                codec_params: codec_params(&codec_name, level),
                stored_size_bytes: stored_size,
                reconstructs_original: fidelity == Fidelity::Exact,
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

        if let Some(hook) = &self.write_hook {
            hook(WriteEvent {
                object_id,
                tenant: self.policy.tenant.clone(),
                content_type: content_type.to_string(),
                bytes: data.to_vec(),
            });
        }
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

    /// Fetch a manifest by object_id, without reading the bytes. Used by the
    /// native door's content-by-hash and introspection endpoints.
    pub fn object_manifest(&self, object_id: &Hash) -> Result<Option<Manifest>> {
        Ok(self.store.manifests.get(object_id)?)
    }

    pub fn delete(&self, bucket: &str, key: &str) -> Result<()> {
        Ok(self.store.pointers.remove(bucket, key)?)
    }

    /// Buckets that currently hold at least one key.
    pub fn buckets(&self) -> Result<Vec<String>> {
        Ok(self.store.pointers.buckets()?)
    }

    /// Keys currently present in a bucket.
    pub fn keys(&self, bucket: &str) -> Result<Vec<String>> {
        Ok(self.store.pointers.list(bucket)?)
    }

    pub fn bucket_config(&self, bucket: &str) -> Result<barme_core::BucketConfig> {
        Ok(self.store.meta.config(bucket)?)
    }

    pub fn set_bucket_config(
        &self,
        bucket: &str,
        config: &barme_core::BucketConfig,
    ) -> Result<()> {
        Ok(self.store.meta.set_config(bucket, config)?)
    }

    /// Whether a bucket allows anonymous reads.
    pub fn is_public(&self, bucket: &str) -> Result<bool> {
        Ok(self.bucket_config(bucket)?.public_read)
    }

    /// Rename a bucket (pointers + config). No object data moves.
    pub fn rename_bucket(&self, old: &str, new: &str) -> Result<()> {
        self.store.pointers.rename_bucket(old, new)?;
        self.store.meta.rename_bucket(old, new)?;
        Ok(())
    }

    /// Delete a bucket and all its pointers. Chunks are reclaimed by GC.
    pub fn delete_bucket(&self, bucket: &str) -> Result<()> {
        self.store.pointers.delete_bucket(bucket)?;
        self.store.meta.delete_bucket(bucket)?;
        Ok(())
    }

    /// Move an object to a new bucket/key, keeping its version history. Returns
    /// false if the source doesn't exist.
    pub fn move_object(&self, fb: &str, fk: &str, tb: &str, tk: &str) -> Result<bool> {
        Ok(self.store.pointers.move_key(fb, fk, tb, tk)?)
    }

    /// Copy an object's current version to a new bucket/key. Chunks are shared,
    /// so this costs a pointer, not the bytes. Returns false if the source
    /// doesn't exist.
    pub fn copy_object(&self, fb: &str, fk: &str, tb: &str, tk: &str) -> Result<bool> {
        Ok(self.store.pointers.copy(fb, fk, tb, tk)?)
    }

    /// Run one garbage-collection sweep. `now_secs` is injected so the caller
    /// owns the clock; `grace` is how long a chunk stays condemned before it's
    /// erased. Returns what the pass did.
    pub fn gc_sweep(&self, now_secs: u64, grace: std::time::Duration) -> Result<barme_gc::Sweep> {
        Ok(barme_gc::Gc::new(&self.store, grace).sweep(now_secs)?)
    }

    /// Storage-wide statistics. `logical_bytes` is what users think they stored
    /// (sum of current object sizes); `physical_bytes` is what's actually on
    /// disk after dedup and compression. The gap is the win.
    pub fn stats(&self) -> Result<Stats> {
        let buckets = self.store.pointers.buckets()?;
        let mut objects = 0usize;
        let mut logical_bytes = 0u64;
        for b in &buckets {
            for k in self.keys(b)? {
                if let Some(m) = self.manifest(b, &k)? {
                    objects += 1;
                    logical_bytes += m.original.size_bytes;
                }
            }
        }
        Ok(Stats {
            buckets: buckets.len(),
            objects,
            logical_bytes,
            physical_bytes: self.store.chunks.physical_bytes()?,
            unique_chunks: self.store.chunks.count()?,
        })
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

}

fn build_codec(name: &str, level: i32) -> Result<Box<dyn Codec>> {
    match name {
        "none" => Ok(Box::new(Raw)),
        "zstd" => Ok(Box::new(Zstd::new(level))),
        other => Err(CodecError::Unknown(other.to_string()).into()),
    }
}

fn codec_params(name: &str, level: i32) -> serde_json::Value {
    match name {
        "zstd" => serde_json::json!({ "level": level }),
        _ => serde_json::json!({}),
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
