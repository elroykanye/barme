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
    Annotation, Chunking, Fidelity, Hash, Manifest, Original, Quality, Route, Storage, Webhook,
    MANIFEST_VERSION,
};
use barme_store::{Store, StoreError};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;

/// Emitted after a successful write, for anything that wants to react to new
/// objects (the semantic layer and webhooks). Handed to the write hook by value.
pub struct WriteEvent {
    pub object_id: Hash,
    pub tenant: String,
    pub content_type: String,
    /// Where the write landed, so reactors can annotate or report the location.
    pub bucket: String,
    pub key: String,
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
    /// A write or delete was refused because the object is locked until a time
    /// still in the future.
    #[error("locked: {0}/{1} is locked until {2}")]
    Locked(String, String, String),
    /// The key is empty, or the pot and key together are too long to store.
    /// The stores encode `(pot, key)` into a single filename, whose length is
    /// bounded by the filesystem's filename limit; see [`MAX_NAME_BYTES`].
    #[error("invalid key: {0}")]
    InvalidKey(String),
}

/// Filesystem filename-length limit, in bytes. The stores hex-encode `(pot,
/// key)` into a single filename; the longest form is the annotation store's
/// `{hexpot}_{hexkey}.json`. That has to fit here, which is what bounds how
/// long a pot name plus key can be. Past it a write would fail deep in the
/// store with an opaque I/O error, so we reject it up front instead.
pub const MAX_NAME_BYTES: usize = 255;

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

/// The result of comparing two object versions by their chunk sets.
#[derive(Debug, Clone, Copy, serde::Serialize)]
pub struct Diff {
    pub added: usize,
    pub removed: usize,
    pub shared: usize,
}

/// The actionable form of a diff: the exact chunks a sync from `from` to `to`
/// would transfer (`add`) and the ones `to` no longer uses (`remove`).
#[derive(Debug, Clone, serde::Serialize)]
pub struct Delta {
    pub root: Hash,
    pub add: Vec<Hash>,
    pub remove: Vec<Hash>,
}

/// A Merkle inclusion proof for one chunk of one object.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ChunkProof {
    pub object_id: Hash,
    pub root: Hash,
    pub chunk: Hash,
    pub proof: barme_core::merkle::Proof,
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
        validate_key(bucket, key)?;
        self.ensure_unlocked(bucket, key)?;
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
            let encoded = codec.encode(c.data)?;
            stored_size += encoded.len() as u64;
            chunks.push(self.store.chunks.put(&encoded)?);
        }
        let merkle_root = Some(barme_core::merkle::root(&chunks));

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
                merkle_root,
            },
            quality: Quality::default(),
            tenant: self.policy.tenant.clone(),
            policy_snapshot: self.policy.policy_name.clone(),
        };

        let object_id = self.store.manifests.put(&manifest)?;
        self.store.pointers.set(bucket, key, &object_id)?;
        // Record where this object_id lives, so a semantic hit can name a
        // location and auto-tagging can find the object to annotate.
        self.store.reverse.add(&object_id, bucket, key)?;

        if let Some(hook) = &self.write_hook {
            hook(WriteEvent {
                object_id,
                tenant: self.policy.tenant.clone(),
                content_type: content_type.to_string(),
                bucket: bucket.to_string(),
                key: key.to_string(),
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
        self.ensure_unlocked(bucket, key)?;
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

    // ---- access keys ----

    pub fn list_keys(&self) -> Result<Vec<barme_core::KeyRecord>> {
        Ok(self.store.keys.list()?)
    }

    pub fn get_key(&self, access_key: &str) -> Result<Option<barme_core::KeyRecord>> {
        Ok(self.store.keys.get(access_key)?)
    }

    pub fn create_key(&self, record: &barme_core::KeyRecord) -> Result<()> {
        let mut r = record.clone();
        if r.created_at.is_empty() {
            r.created_at = now_rfc3339();
        }
        Ok(self.store.keys.put(&r)?)
    }

    pub fn delete_key(&self, access_key: &str) -> Result<()> {
        Ok(self.store.keys.delete(access_key)?)
    }

    /// Seed a full-owner key if the store has none. Used to bootstrap the
    /// configured owner credential on first run.
    pub fn ensure_owner(&self, access_key: &str, secret_key: &str) -> Result<()> {
        if self.store.keys.list()?.is_empty() {
            self.store.keys.put(&barme_core::KeyRecord {
                access_key: access_key.to_string(),
                secret_key: secret_key.to_string(),
                read_only: false,
                pots: vec![],
                created_at: now_rfc3339(),
            })?;
        }
        Ok(())
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

    /// Apply per-pot lifecycle rules: expire objects older than the pot's
    /// `expire_after_days`, and trim each key to `max_versions`. `now_secs` is
    /// injected so the caller owns the clock.
    pub fn enforce_lifecycle(&self, now_secs: u64) -> Result<()> {
        for pot in self.store.pointers.buckets()? {
            let cfg = self.store.meta.config(&pot)?;
            let max_versions = cfg.max_versions.unwrap_or(0);
            let expire_days = cfg.expire_after_days.unwrap_or(0);
            if max_versions == 0 && expire_days == 0 {
                continue;
            }
            for key in self.store.pointers.list(&pot)? {
                if expire_days > 0 {
                    if let Some(m) = self.manifest(&pot, &key)? {
                        if let Some(created) = parse_rfc3339_secs(&m.created_at) {
                            if now_secs.saturating_sub(created) > (expire_days as u64) * 86_400 {
                                self.store.pointers.remove(&pot, &key)?;
                                continue;
                            }
                        }
                    }
                }
                if max_versions > 0 {
                    self.store.pointers.trim(&pot, &key, max_versions as usize)?;
                }
            }
        }
        Ok(())
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

    // ---- annotations + locking ----

    /// The object's user annotation (tags, note, favorite, lock). Empty default
    /// if never set.
    pub fn annotation(&self, bucket: &str, key: &str) -> Result<Annotation> {
        Ok(self.store.annotations.get(bucket, key)?)
    }

    pub fn set_annotation(&self, bucket: &str, key: &str, annotation: &Annotation) -> Result<()> {
        Ok(self.store.annotations.set(bucket, key, annotation)?)
    }

    /// Refuse if the object is locked until a time still in the future.
    fn ensure_unlocked(&self, bucket: &str, key: &str) -> Result<()> {
        if let Some(until) = self.store.annotations.get(bucket, key)?.locked_until {
            if let Some(until_secs) = parse_rfc3339_secs(&until) {
                if until_secs > now_unix() {
                    return Err(EngineError::Locked(bucket.into(), key.into(), until));
                }
            }
        }
        Ok(())
    }

    // ---- versions, diff, verify ----

    /// Roll a key's pointer forward to an existing manifest (an older version,
    /// usually). The manifest must already be in the store. Honors the lock.
    pub fn restore_version(&self, bucket: &str, key: &str, object_id: &Hash) -> Result<()> {
        self.ensure_unlocked(bucket, key)?;
        if self.store.manifests.get(object_id)?.is_none() {
            return Err(EngineError::DanglingPointer(*object_id));
        }
        self.store.pointers.set(bucket, key, object_id)?;
        self.store.reverse.add(object_id, bucket, key)?;
        Ok(())
    }

    /// Compare two manifests by their chunk sets: how many chunks `b` adds over
    /// `a`, how many it drops, and how many they share.
    pub fn diff(&self, a: &Hash, b: &Hash) -> Result<Diff> {
        let ma = self
            .store
            .manifests
            .get(a)?
            .ok_or(EngineError::DanglingPointer(*a))?;
        let mb = self
            .store
            .manifests
            .get(b)?
            .ok_or(EngineError::DanglingPointer(*b))?;
        let sa: HashSet<Hash> = ma.chunking.chunks.into_iter().collect();
        let sb: HashSet<Hash> = mb.chunking.chunks.into_iter().collect();
        Ok(Diff {
            added: sb.difference(&sa).count(),
            removed: sa.difference(&sb).count(),
            shared: sa.intersection(&sb).count(),
        })
    }

    /// Re-read the current version and check it reassembles to the digest the
    /// manifest recorded. Integrity failures come back as `Ok(false)`, not an
    /// error, so a caller can report a bad object without the request failing.
    /// A key with no current version is `Ok(false)`.
    pub fn verify(&self, bucket: &str, key: &str) -> Result<bool> {
        let Some(object_id) = self.store.pointers.current(bucket, key)? else {
            return Ok(false);
        };
        let manifest = self
            .store
            .manifests
            .get(&object_id)?
            .ok_or(EngineError::DanglingPointer(object_id))?;
        match self.read_manifest_bytes(&object_id) {
            Ok(bytes) => Ok(sha256_hex(&bytes) == manifest.original.sha256),
            Err(EngineError::Integrity(_)) => Ok(false),
            Err(EngineError::Store(StoreError::Integrity { .. })) => Ok(false),
            Err(e) => Err(e),
        }
    }

    // ---- merkle: roots, inclusion proofs, sync deltas ----

    /// The Merkle root committing to an object's ordered chunks. Read from the
    /// manifest, recomputed if the manifest predates the field.
    pub fn object_root(&self, object_id: &Hash) -> Result<Hash> {
        let m = self
            .store
            .manifests
            .get(object_id)?
            .ok_or(EngineError::DanglingPointer(*object_id))?;
        Ok(m.chunking
            .merkle_root
            .unwrap_or_else(|| barme_core::merkle::root(&m.chunking.chunks)))
    }

    /// An inclusion proof that the chunk at `index` belongs to the current
    /// version of a key. `None` if the key or that chunk index doesn't exist.
    pub fn prove_chunk(&self, bucket: &str, key: &str, index: usize) -> Result<Option<ChunkProof>> {
        let Some(object_id) = self.store.pointers.current(bucket, key)? else {
            return Ok(None);
        };
        let m = self
            .store
            .manifests
            .get(&object_id)?
            .ok_or(EngineError::DanglingPointer(object_id))?;
        let Some(proof) = barme_core::merkle::prove(&m.chunking.chunks, index) else {
            return Ok(None);
        };
        let root = m
            .chunking
            .merkle_root
            .unwrap_or_else(|| barme_core::merkle::root(&m.chunking.chunks));
        Ok(Some(ChunkProof {
            object_id,
            root,
            chunk: m.chunking.chunks[index],
            proof,
        }))
    }

    /// The chunk-level delta from `from` to `to`: which chunks `to` adds that
    /// `from` lacks (what a sync would carry) and which it drops.
    pub fn delta(&self, from: &Hash, to: &Hash) -> Result<Delta> {
        let mf = self
            .store
            .manifests
            .get(from)?
            .ok_or(EngineError::DanglingPointer(*from))?;
        let mt = self
            .store
            .manifests
            .get(to)?
            .ok_or(EngineError::DanglingPointer(*to))?;
        let have: HashSet<Hash> = mf.chunking.chunks.iter().copied().collect();
        let want: HashSet<Hash> = mt.chunking.chunks.iter().copied().collect();
        Ok(Delta {
            root: mt
                .chunking
                .merkle_root
                .unwrap_or_else(|| barme_core::merkle::root(&mt.chunking.chunks)),
            add: mt
                .chunking
                .chunks
                .iter()
                .copied()
                .filter(|h| !have.contains(h))
                .collect(),
            remove: mf
                .chunking
                .chunks
                .iter()
                .copied()
                .filter(|h| !want.contains(h))
                .collect(),
        })
    }

    // ---- sync primitives: replicate an object between stores ----

    /// Is this chunk present locally?
    pub fn has_chunk(&self, hash: &Hash) -> bool {
        self.store.chunks.has(hash)
    }

    /// Of `wanted`, the chunks this store lacks. A puller sends its target
    /// manifest's chunk list; the answer is exactly what to fetch.
    pub fn missing_chunks(&self, wanted: &[Hash]) -> Vec<Hash> {
        wanted
            .iter()
            .copied()
            .filter(|h| !self.store.chunks.has(h))
            .collect()
    }

    /// Raw stored bytes of a chunk (compressed as written), by hash, to ship it
    /// to another store verbatim.
    pub fn chunk_bytes(&self, hash: &Hash) -> Result<Option<Vec<u8>>> {
        Ok(self.store.chunks.get(hash)?)
    }

    /// Store raw chunk bytes received from another store. Content-addressed, so
    /// the bytes verify themselves; returns the address.
    pub fn put_chunk_bytes(&self, bytes: &[u8]) -> Result<Hash> {
        Ok(self.store.chunks.put(bytes)?)
    }

    /// Adopt a manifest fetched from another store and point `bucket/key` at it.
    /// Refuses unless every chunk it names is already present and its declared
    /// object_id and Merkle root check out. Returns the object_id.
    pub fn import_object(
        &self,
        bucket: &str,
        key: &str,
        manifest: &barme_core::Manifest,
    ) -> Result<Hash> {
        self.ensure_unlocked(bucket, key)?;
        for h in &manifest.chunking.chunks {
            if !self.store.chunks.has(h) {
                return Err(EngineError::MissingChunk(*h, manifest.object_id));
            }
        }
        if let Some(root) = manifest.chunking.merkle_root {
            if root != barme_core::merkle::root(&manifest.chunking.chunks) {
                return Err(EngineError::Integrity(manifest.object_id));
            }
        }
        // manifests.put re-derives the id from content; a mismatch means the
        // manifest was altered in transit.
        let object_id = self.store.manifests.put(manifest)?;
        if object_id != manifest.object_id {
            return Err(EngineError::Integrity(manifest.object_id));
        }
        self.store.pointers.set(bucket, key, &object_id)?;
        self.store.reverse.add(&object_id, bucket, key)?;
        Ok(object_id)
    }

    // ---- reverse index ----

    /// Pots/keys that point at an object_id, insertion order. Empty if unknown.
    pub fn locations(&self, object_id: &Hash) -> Result<Vec<(String, String)>> {
        Ok(self.store.reverse.get(object_id)?)
    }

    // ---- presign ----

    /// The server signing secret: the first owner key's secret, or None if there
    /// are no keys (open mode) or none is a full owner.
    pub fn signing_secret(&self) -> Option<String> {
        self.store
            .keys
            .list()
            .ok()?
            .into_iter()
            .find(|k| k.is_owner())
            .map(|k| k.secret_key)
    }

    // ---- webhooks ----

    pub fn list_webhooks(&self) -> Result<Vec<Webhook>> {
        Ok(self.store.webhooks.list()?)
    }

    pub fn add_webhook(&self, hook: &Webhook) -> Result<()> {
        Ok(self.store.webhooks.put(hook)?)
    }

    pub fn delete_webhook(&self, id: &str) -> Result<()> {
        Ok(self.store.webhooks.delete(id)?)
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

/// Reject keys the store can't hold before we do any work. An empty key has no
/// filename; a `(pot, key)` pair whose encoded filename would overflow the
/// filesystem's filename limit is refused with a clear message rather than a
/// mid-write I/O error. The worst-case encoding is the annotation store's
/// `{hexpot}_{hexkey}.json` — two hex chars per byte, a `_` separator, and the
/// `.json` suffix.
fn validate_key(bucket: &str, key: &str) -> Result<()> {
    if key.is_empty() {
        return Err(EngineError::InvalidKey("key must not be empty".into()));
    }
    let encoded = 2 * bucket.len() + 2 * key.len() + "_.json".len();
    if encoded > MAX_NAME_BYTES {
        return Err(EngineError::InvalidKey(format!(
            "pot+key encode to a {encoded}-byte filename; the limit is {MAX_NAME_BYTES}"
        )));
    }
    Ok(())
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

fn parse_rfc3339_secs(s: &str) -> Option<u64> {
    use time::format_description::well_known::Rfc3339;
    time::OffsetDateTime::parse(s, &Rfc3339)
        .ok()
        .map(|t| t.unix_timestamp().max(0) as u64)
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
