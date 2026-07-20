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
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

/// Number of pointer-commit lock shards. A key maps to one shard, so writes to
/// the same key serialize while writes to different keys almost always don't.
const KEY_LOCK_SHARDS: usize = 256;

/// Emitted after a successful write, for anything that wants to react to new
/// objects (the semantic layer and webhooks). Carries only the object's
/// identity and location — never its bytes. A reactor that needs the content
/// (the semantic indexer) reads it back by `object_id`, off the write path, so
/// a large streamed upload never has to be re-materialized in memory just to
/// fire the hook.
pub struct WriteEvent {
    pub object_id: Hash,
    pub tenant: String,
    pub content_type: String,
    /// Where the write landed, so reactors can annotate or report the location.
    pub bucket: String,
    pub key: String,
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
    /// A streaming upload exceeded the caller-supplied size limit. Any chunks
    /// already written are left unreferenced for GC to reclaim.
    #[error("upload too large: exceeds {limit} bytes")]
    TooLarge { limit: u64 },
    /// Reading the upload stream failed partway (client disconnect, truncated
    /// body, or an I/O error on the socket).
    #[error("reading upload: {0}")]
    Upload(#[source] std::io::Error),
    /// A multipart operation named an upload id the server doesn't know: never
    /// created, already completed or aborted, or lost to a process restart.
    #[error("no such upload: {0}")]
    NoSuchUpload(String),
    /// CompleteMultipartUpload named a part number that was never uploaded.
    #[error("invalid part: {0} was not uploaded")]
    InvalidPart(u32),
}

impl EngineError {
    /// True when the error is caused by malformed client input — a bad key or a
    /// bad pot name — so the front doors can answer 400 instead of a misleading
    /// 500. A pot name with a slash or `..` is rejected deep in the store as
    /// `BadBucket`; that's the caller's fault, not the server's.
    pub fn is_bad_input(&self) -> bool {
        matches!(
            self,
            EngineError::InvalidKey(_)
                | EngineError::InvalidPart(_)
                | EngineError::Store(StoreError::BadBucket(_))
        )
    }
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
    /// Per-key commit locks. The pointer file is read-modify-write (read the
    /// history, append a version, rewrite), so two concurrent writers to one key
    /// would otherwise clobber each other and drop versions. A key hashes to one
    /// shard; holding it across the commit serializes same-key writers only.
    key_locks: Vec<Mutex<()>>,
    /// In-progress multipart uploads, keyed by upload id. State is in memory
    /// only: a restart abandons them, and their already-stored part chunks are
    /// unreferenced, so GC reclaims them like any other orphan. Chunks are pinned
    /// while staged so a sweep can't erase them before the upload completes.
    multipart: Mutex<HashMap<String, StagedUpload>>,
    /// Mixed into new upload ids so two uploads to the same key in the same
    /// second still get distinct ids.
    mp_counter: AtomicU64,
}

impl Engine {
    pub fn open(root: impl AsRef<Path>, policy: Policy) -> Result<Self> {
        Ok(Engine {
            store: Store::open(root)?,
            policy,
            write_hook: None,
            key_locks: (0..KEY_LOCK_SHARDS).map(|_| Mutex::new(())).collect(),
            multipart: Mutex::new(HashMap::new()),
            mp_counter: AtomicU64::new(0),
        })
    }

    /// Open with a 32-byte master key so access-key secrets are encrypted at
    /// rest. Everything else is identical to [`open`](Self::open); the key only
    /// affects the key store. Legacy plaintext key records are migrated on open.
    pub fn open_encrypted(
        root: impl AsRef<Path>,
        policy: Policy,
        master_key: &[u8; 32],
    ) -> Result<Self> {
        Ok(Engine {
            store: Store::open_encrypted(root, master_key)?,
            policy,
            write_hook: None,
            key_locks: (0..KEY_LOCK_SHARDS).map(|_| Mutex::new(())).collect(),
            multipart: Mutex::new(HashMap::new()),
            mp_counter: AtomicU64::new(0),
        })
    }

    /// Lock the commit shard for a key. Held across the pointer read-modify-write
    /// so concurrent writes to the same key can't lose a version; different keys
    /// hash to different shards and run in parallel.
    fn key_lock(&self, bucket: &str, key: &str) -> MutexGuard<'_, ()> {
        use std::hash::{Hash as _, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        bucket.hash(&mut h);
        0u8.hash(&mut h); // separator so ("ab","c") and ("a","bc") differ
        key.hash(&mut h);
        let idx = (h.finish() as usize) % self.key_locks.len();
        // A poisoned lock only means a prior writer panicked mid-commit; the
        // pointer write is atomic, so the data is consistent — take it anyway.
        self.key_locks[idx]
            .lock()
            .unwrap_or_else(|p| p.into_inner())
    }

    /// Temp files reaped on open because the previous run was killed mid-write.
    /// Zero after a clean shutdown; non-zero is a benign crash-recovery signal.
    pub fn recovered_temp(&self) -> usize {
        self.store.recovered_temp
    }

    /// The on-disk layout version this data directory is stamped with.
    pub fn format_version(&self) -> u32 {
        self.store.format_version
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
        let ep = self.effective_policy(bucket, content_type)?;

        let mut chunks = Vec::new();
        let mut stored_size = 0u64;
        let mut pins = PinGuard::new(&self.store.chunks);
        for c in barme_chunk::chunk(data) {
            let encoded = ep.codec.encode(c.data)?;
            stored_size += encoded.len() as u64;
            let h = self.store.chunks.put(&encoded)?;
            pins.pin(h);
            chunks.push(h);
        }

        let object_id = {
            let _commit = self.key_lock(bucket, key);
            self.finalize_write(
                bucket,
                key,
                content_type,
                &ep,
                chunks,
                stored_size,
                data.len() as u64,
                sha256_hex(data),
            )?
        };

        if let Some(hook) = &self.write_hook {
            hook(WriteEvent {
                object_id,
                tenant: self.policy.tenant.clone(),
                content_type: content_type.to_string(),
                bucket: bucket.to_string(),
                key: key.to_string(),
            });
        }
        Ok(object_id)
    }

    /// Streaming write: chunk the object as it arrives from `reader`, storing
    /// each chunk as it's cut, so memory stays flat regardless of object size.
    /// `max_bytes` caps the total; past it the write is abandoned with
    /// [`EngineError::TooLarge`] and the chunks written so far are left
    /// unreferenced for GC to reclaim.
    ///
    /// Produces byte-for-byte the same object as [`put`](Self::put) on the same
    /// input — same chunk boundaries, same manifest, same `object_id` — so a
    /// streamed upload dedups against a buffered one.
    pub fn put_stream<R: std::io::Read>(
        &self,
        bucket: &str,
        key: &str,
        reader: R,
        content_type: &str,
        max_bytes: u64,
    ) -> Result<Hash> {
        validate_key(bucket, key)?;
        self.ensure_unlocked(bucket, key)?;
        let ep = self.effective_policy(bucket, content_type)?;

        let mut chunks = Vec::new();
        let mut stored_size = 0u64;
        let mut orig_size = 0u64;
        let mut hasher = Sha256::new();
        // Pin each chunk as it's stored so GC treats the whole in-flight object
        // as reachable until the pointer commits. The guard unpins on any exit,
        // including the TooLarge abort below and a panic.
        let mut pins = PinGuard::new(&self.store.chunks);

        for item in barme_chunk::chunk_stream(reader) {
            let (_raw_hash, data) = item.map_err(EngineError::Upload)?;
            orig_size += data.len() as u64;
            if orig_size > max_bytes {
                return Err(EngineError::TooLarge { limit: max_bytes });
            }
            hasher.update(&data);
            let encoded = ep.codec.encode(&data)?;
            stored_size += encoded.len() as u64;
            let h = self.store.chunks.put(&encoded)?;
            pins.pin(h);
            chunks.push(h);
        }
        let sha256 = hex::encode(hasher.finalize());

        let object_id = {
            let _commit = self.key_lock(bucket, key);
            self.finalize_write(bucket, key, content_type, &ep, chunks, stored_size, orig_size, sha256)?
        };

        if let Some(hook) = &self.write_hook {
            // No bytes in the event: a reactor that needs the content reads it
            // back by id. This keeps the streaming write flat in memory — the
            // whole point of it — instead of re-materializing the object here.
            hook(WriteEvent {
                object_id,
                tenant: self.policy.tenant.clone(),
                content_type: content_type.to_string(),
                bucket: bucket.to_string(),
                key: key.to_string(),
            });
        }
        Ok(object_id)
    }

    // ---- multipart upload ----

    /// Begin a multipart upload. Validates the key and lock up front and
    /// snapshots the pot's effective policy, so every part encodes exactly the
    /// way the final manifest will record even if the pot's policy changes
    /// mid-upload. Returns an opaque upload id.
    pub fn create_multipart(&self, bucket: &str, key: &str, content_type: &str) -> Result<String> {
        validate_key(bucket, key)?;
        self.ensure_unlocked(bucket, key)?;
        let ep = self.effective_policy(bucket, content_type)?;
        let n = self.mp_counter.fetch_add(1, Ordering::Relaxed);
        let upload_id = sha256_hex(format!("{bucket}/{key}/{}/{n}", now_unix()).as_bytes());
        let staged = StagedUpload {
            bucket: bucket.to_string(),
            key: key.to_string(),
            content_type: content_type.to_string(),
            codec_name: ep.codec_name,
            level: ep.level,
            fidelity: ep.fidelity,
            route: ep.route,
            parts: BTreeMap::new(),
            pinned: Vec::new(),
        };
        self.multipart.lock().unwrap().insert(upload_id.clone(), staged);
        Ok(upload_id)
    }

    /// Stream one part into the store. Chunks are cut and stored exactly as in a
    /// single streaming write, then pinned so GC leaves them alone until the
    /// upload completes or aborts. Re-uploading a part number replaces its
    /// record; the superseded chunks stay pinned until the whole upload
    /// finalizes, then fall out of reach and GC reclaims them. Returns the part's
    /// ETag (hex SHA-256 of its original bytes) and size.
    pub fn upload_part<R: std::io::Read>(
        &self,
        upload_id: &str,
        part_number: u32,
        reader: R,
        max_bytes: u64,
    ) -> Result<PartMeta> {
        // Snapshot the codec for this upload; fail fast on an unknown id.
        let (codec_name, level) = {
            let map = self.multipart.lock().unwrap();
            let up = map
                .get(upload_id)
                .ok_or_else(|| EngineError::NoSuchUpload(upload_id.to_string()))?;
            (up.codec_name.clone(), up.level)
        };
        let codec = build_codec(&codec_name, level)?;

        // Pin each chunk as it lands through a guard, so any early return in this
        // loop — a client disconnect (`Upload`), an encode/store error, or the
        // size cap — releases the pins instead of leaking them. On success the
        // pins are handed to the staged upload (`disarm`), which unpins them at
        // complete/abort.
        let mut pins = PinGuard::new(&self.store.chunks);
        let mut chunks = Vec::new();
        let mut stored_size = 0u64;
        let mut orig_size = 0u64;
        let mut hasher = Sha256::new();
        for item in barme_chunk::chunk_stream(reader) {
            let (_raw_hash, data) = item.map_err(EngineError::Upload)?;
            orig_size += data.len() as u64;
            if orig_size > max_bytes {
                return Err(EngineError::TooLarge { limit: max_bytes });
            }
            hasher.update(&data);
            let encoded = codec.encode(&data)?;
            stored_size += encoded.len() as u64;
            let h = self.store.chunks.put(&encoded)?;
            pins.pin(h);
            chunks.push(h);
        }
        let etag = hex::encode(hasher.finalize());

        let mut map = self.multipart.lock().unwrap();
        match map.get_mut(upload_id) {
            Some(up) => {
                up.pinned.extend(chunks.iter().copied());
                up.parts.insert(
                    part_number,
                    StagedPart { chunks, stored_size, orig_size, etag: etag.clone() },
                );
                // The staged upload owns the pins now; don't unpin on drop.
                pins.disarm();
                Ok(PartMeta { etag, size: orig_size })
            }
            None => {
                // Aborted out from under us mid-part; `pins` drops here and
                // unpins, so the orphaned chunks are reclaimable.
                Err(EngineError::NoSuchUpload(upload_id.to_string()))
            }
        }
    }

    /// Finish a multipart upload: concatenate the named parts' chunks in order,
    /// re-hash the whole object to recover its original-bytes digest (part
    /// digests don't compose into it), write one manifest, and move the pointer.
    /// `order` is the client's part list; empty means every staged part,
    /// ascending. Returns the object id, which is also the completed ETag.
    pub fn complete_multipart(&self, upload_id: &str, order: &[u32]) -> Result<Hash> {
        // Take the upload out up front so a second concurrent complete can't
        // double-commit. On any error we don't reinsert; the client retries fresh.
        let mut staged = self
            .multipart
            .lock()
            .unwrap()
            .remove(upload_id)
            .ok_or_else(|| EngineError::NoSuchUpload(upload_id.to_string()))?;

        // Release the in-flight pins on every exit from here on. On success the
        // chunks are reachable through the new pointer; on any failure below
        // (bad part number, missing chunk, decode/commit error) they're orphans.
        // Either way the pins must come off — a bare `?` return must not leak
        // them, and the upload is already out of the map so it can't be aborted.
        let _release = ReleasePins {
            chunks: &self.store.chunks,
            hashes: std::mem::take(&mut staged.pinned),
        };

        let order: Vec<u32> = if order.is_empty() {
            staged.parts.keys().copied().collect()
        } else {
            order.to_vec()
        };

        let decoder = barme_codec::decoder_for(&staged.codec_name)?;
        let mut all_chunks: Vec<Hash> = Vec::new();
        let mut orig_size = 0u64;
        let mut stored_size = 0u64;
        let mut hasher = Sha256::new();
        for pn in &order {
            let part = staged.parts.get(pn).ok_or(EngineError::InvalidPart(*pn))?;
            for h in &part.chunks {
                let stored = self
                    .store
                    .chunks
                    .get(h)?
                    .ok_or(EngineError::MissingChunk(*h, Hash::of(b"")))?;
                hasher.update(&decoder.decode(&stored)?);
                all_chunks.push(*h);
            }
            orig_size += part.orig_size;
            stored_size += part.stored_size;
        }
        let sha256 = hex::encode(hasher.finalize());

        let ep = EffectivePolicy {
            codec: build_codec(&staged.codec_name, staged.level)?,
            codec_name: staged.codec_name.clone(),
            level: staged.level,
            fidelity: staged.fidelity,
            route: staged.route,
        };

        let object_id = {
            let _commit = self.key_lock(&staged.bucket, &staged.key);
            self.finalize_write(
                &staged.bucket,
                &staged.key,
                &staged.content_type,
                &ep,
                all_chunks,
                stored_size,
                orig_size,
                sha256,
            )?
        };

        // `_release` drops at end of scope and unpins the staged chunks — now
        // reachable through the pointer, so unpinning is safe.

        if let Some(hook) = &self.write_hook {
            hook(WriteEvent {
                object_id,
                tenant: self.policy.tenant.clone(),
                content_type: staged.content_type.clone(),
                bucket: staged.bucket.clone(),
                key: staged.key.clone(),
            });
        }
        Ok(object_id)
    }

    /// Abandon a multipart upload. Idempotent: an unknown id is a no-op. The
    /// staged part chunks lose their pins and become GC-eligible orphans.
    pub fn abort_multipart(&self, upload_id: &str) -> Result<()> {
        if let Some(staged) = self.multipart.lock().unwrap().remove(upload_id) {
            self.store.chunks.unpin(&staged.pinned);
        }
        Ok(())
    }

    /// The parts staged so far for an upload, ascending, for ListParts. `None`
    /// if the upload id is unknown.
    pub fn list_parts(&self, upload_id: &str) -> Result<Option<MultipartListing>> {
        let map = self.multipart.lock().unwrap();
        let Some(up) = map.get(upload_id) else {
            return Ok(None);
        };
        let parts = up
            .parts
            .iter()
            .map(|(n, p)| (*n, PartMeta { etag: p.etag.clone(), size: p.orig_size }))
            .collect();
        Ok(Some(MultipartListing {
            bucket: up.bucket.clone(),
            key: up.key.clone(),
            parts,
        }))
    }

    /// How many chunks are currently pinned as in-flight — uncommitted single
    /// writes plus staged multipart parts. It returns to zero once nothing is
    /// mid-upload, so a non-zero count with no active upload is a pin leak. Used
    /// by tests and available for ops introspection.
    pub fn pinned_chunk_count(&self) -> usize {
        self.store.chunks.pinned().len()
    }

    /// The pot's effective storage policy: its own overrides where set, the
    /// server default otherwise. Shared by the buffered and streaming writes.
    fn effective_policy(&self, bucket: &str, content_type: &str) -> Result<EffectivePolicy> {
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
        Ok(EffectivePolicy {
            codec,
            codec_name,
            level,
            fidelity,
            route,
        })
    }

    /// Assemble the manifest from an already-stored chunk set and commit it:
    /// write the manifest, move the pointer, record the reverse index. Shared
    /// tail of both write paths. Does not fire the write hook — the caller does,
    /// since only it knows whether the object bytes are still in hand.
    #[allow(clippy::too_many_arguments)]
    fn finalize_write(
        &self,
        bucket: &str,
        key: &str,
        content_type: &str,
        ep: &EffectivePolicy,
        chunks: Vec<Hash>,
        stored_size: u64,
        orig_size: u64,
        sha256: String,
    ) -> Result<Hash> {
        let merkle_root = Some(barme_core::merkle::root(&chunks));
        let manifest = Manifest {
            manifest_version: MANIFEST_VERSION,
            object_id: Hash::of(b""), // set by the manifest store
            created_at: now_rfc3339(),
            original: Original {
                size_bytes: orig_size,
                sha256,
                content_type: content_type.to_string(),
            },
            storage: Storage {
                route: ep.route,
                fidelity: ep.fidelity,
                codec: ep.codec_name.clone(),
                codec_params: codec_params(&ep.codec_name, ep.level),
                stored_size_bytes: stored_size,
                reconstructs_original: ep.fidelity == Fidelity::Exact,
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

    /// What a streaming reader needs before pulling bytes: content type, total
    /// size, codec, and the ordered chunk addresses. `None` if the key is
    /// absent. Pair with [`read_chunk`](Self::read_chunk) to stream an object
    /// out one chunk at a time, so a download never buffers the whole object.
    pub fn object_head(
        &self,
        bucket: &str,
        key: &str,
    ) -> Result<Option<(String, u64, String, Vec<Hash>)>> {
        let Some(object_id) = self.store.pointers.current(bucket, key)? else {
            return Ok(None);
        };
        let m = self
            .store
            .manifests
            .get(&object_id)?
            .ok_or(EngineError::DanglingPointer(object_id))?;
        Ok(Some((
            m.original.content_type,
            m.original.size_bytes,
            m.storage.codec,
            m.chunking.chunks,
        )))
    }

    /// Read and decode a single chunk by address. The chunk store verifies the
    /// chunk's content hash on read, so a streamed download stays integrity-
    /// checked chunk by chunk without ever holding the whole object.
    pub fn read_chunk(&self, hash: &Hash, codec: &str) -> Result<Vec<u8>> {
        let stored = self
            .store
            .chunks
            .get(hash)?
            .ok_or(EngineError::MissingChunk(*hash, *hash))?;
        let dec = barme_codec::decoder_for(codec)?;
        Ok(dec.decode(&stored)?)
    }

    /// Fetch a manifest by object_id, without reading the bytes. Used by the
    /// native door's content-by-hash and introspection endpoints.
    pub fn object_manifest(&self, object_id: &Hash) -> Result<Option<Manifest>> {
        Ok(self.store.manifests.get(object_id)?)
    }

    pub fn delete(&self, bucket: &str, key: &str) -> Result<()> {
        self.ensure_unlocked(bucket, key)?;
        // Same commit lock as writes: without it a delete can interleave with a
        // concurrent put's read-modify-write and be lost — the put reads the
        // history, the delete removes the file, then the put rewrites it and
        // resurrects the "deleted" key. Serializing makes last-writer-wins clean.
        let _commit = self.key_lock(bucket, key);
        Ok(self.store.pointers.remove(bucket, key)?)
    }

    /// Buckets that currently hold at least one key.
    pub fn buckets(&self) -> Result<Vec<String>> {
        Ok(self.store.pointers.buckets()?)
    }

    /// Explicitly create a pot: persist its default config so the pot is known
    /// even before anything is written to it. Idempotent — creating a pot that
    /// already exists is a no-op, not an error. Writes never need this (a first
    /// PUT to an unknown pot still lands); it exists so S3 clients that provision
    /// a bucket up front, and tooling that lists pots, see what they expect.
    pub fn create_bucket(&self, bucket: &str) -> Result<()> {
        if bucket.is_empty() {
            return Err(EngineError::InvalidKey("pot name must not be empty".into()));
        }
        if !self.store.meta.exists(bucket) {
            self.store
                .meta
                .set_config(bucket, &barme_core::BucketConfig::default())?;
        }
        Ok(())
    }

    /// Whether a pot exists: it was explicitly created (has a config), or it
    /// currently holds objects.
    pub fn bucket_exists(&self, bucket: &str) -> Result<bool> {
        if self.store.meta.exists(bucket) {
            return Ok(true);
        }
        Ok(self.store.pointers.buckets()?.iter().any(|b| b == bucket))
    }

    /// Every pot the store knows: created (has a config) or written to (has
    /// keys), deduplicated and sorted.
    pub fn list_buckets(&self) -> Result<Vec<String>> {
        let mut set: std::collections::BTreeSet<String> =
            self.store.pointers.buckets()?.into_iter().collect();
        set.extend(self.store.meta.list()?);
        Ok(set.into_iter().collect())
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

    /// Delete a bucket and all its pointers. A force delete — used by the native
    /// "delete pot" op. Chunks are reclaimed by GC. Racing this with a write to
    /// the same pot is inherently "which wins"; callers that must not lose a
    /// concurrent write (S3 DeleteBucket) use [`Engine::delete_bucket_if_empty`].
    pub fn delete_bucket(&self, bucket: &str) -> Result<()> {
        self.store.pointers.delete_bucket(bucket)?;
        self.store.meta.delete_bucket(bucket)?;
        Ok(())
    }

    /// Delete a bucket only if it holds no objects, atomically. Returns `false`
    /// (deleting nothing) if it still has objects, so the caller can answer 409.
    ///
    /// Unlike [`Engine::delete_bucket`], this can't lose a concurrent write: a
    /// naive check-then-delete could see an empty bucket, then a racing PUT
    /// commits a pointer, then the delete wipes it — an acknowledged write lost.
    /// Here every commit-lock shard is held across the emptiness check and the
    /// delete, so no pointer can commit in the gap. Writers only ever hold one
    /// shard, so acquiring all of them in index order can't deadlock.
    pub fn delete_bucket_if_empty(&self, bucket: &str) -> Result<bool> {
        let _guards: Vec<MutexGuard<'_, ()>> = self
            .key_locks
            .iter()
            .map(|m| m.lock().unwrap_or_else(|p| p.into_inner()))
            .collect();
        if !self.store.pointers.list(bucket)?.is_empty() {
            return Ok(false);
        }
        self.store.pointers.delete_bucket(bucket)?;
        self.store.meta.delete_bucket(bucket)?;
        Ok(true)
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
        let _commit = self.key_lock(bucket, key);
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
        let codec = barme_codec::decoder_for(&manifest.storage.codec)?;

        // Re-hash chunk by chunk rather than buffering the whole object, so
        // verifying a large object stays flat in memory. Each chunk also self-
        // verifies its content address on read; a missing or corrupted chunk
        // means the object doesn't verify.
        let mut hasher = Sha256::new();
        for h in &manifest.chunking.chunks {
            let stored = match self.store.chunks.get(h) {
                Ok(Some(b)) => b,
                Ok(None) => return Ok(false),
                Err(StoreError::Integrity { .. }) => return Ok(false),
                Err(e) => return Err(e.into()),
            };
            match codec.decode(&stored) {
                Ok(d) => hasher.update(&d),
                Err(_) => return Ok(false),
            }
        }

        // Lossy fidelity intentionally changes bytes, so a digest match isn't
        // expected; chunk presence and per-chunk integrity are the check there.
        if manifest.storage.fidelity == Fidelity::Exact {
            Ok(hex::encode(hasher.finalize()) == manifest.original.sha256)
        } else {
            Ok(true)
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
        let _commit = self.key_lock(bucket, key);
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

/// The resolved storage policy for a single write: the codec to run plus the
/// manifest fields describing it. Computed once by `effective_policy`, then used
/// by both the buffered and streaming write paths.
struct EffectivePolicy {
    codec: Box<dyn Codec>,
    codec_name: String,
    level: i32,
    fidelity: Fidelity,
    route: Route,
}

/// Public metadata for one uploaded part: its ETag (hex SHA-256 of the part's
/// original bytes) and original size in bytes.
#[derive(Debug, Clone)]
pub struct PartMeta {
    pub etag: String,
    pub size: u64,
}

/// What [`Engine::list_parts`] returns: the target pot/key and the staged parts.
#[derive(Debug, Clone)]
pub struct MultipartListing {
    pub bucket: String,
    pub key: String,
    pub parts: Vec<(u32, PartMeta)>,
}

/// One staged part, held in memory between UploadPart and completion.
struct StagedPart {
    chunks: Vec<Hash>,
    stored_size: u64,
    orig_size: u64,
    etag: String,
}

/// An in-progress multipart upload. The codec settings are snapshotted at
/// creation so every part encodes the same way the final manifest records.
struct StagedUpload {
    bucket: String,
    key: String,
    content_type: String,
    codec_name: String,
    level: i32,
    fidelity: Fidelity,
    route: Route,
    parts: BTreeMap<u32, StagedPart>,
    /// Every chunk pinned across the upload's lifetime, released on complete/abort.
    pinned: Vec<Hash>,
}

/// Pins the chunks of an in-flight write so GC treats them as reachable until
/// the pointer commits, and releases every pin on drop — so a `TooLarge` abort,
/// an I/O error, or a panic mid-upload can never strand a chunk pinned forever.
struct PinGuard<'a> {
    chunks: &'a barme_store::ChunkStore,
    pinned: Vec<Hash>,
}

impl<'a> PinGuard<'a> {
    fn new(chunks: &'a barme_store::ChunkStore) -> Self {
        PinGuard {
            chunks,
            pinned: Vec::new(),
        }
    }

    fn pin(&mut self, hash: Hash) {
        self.chunks.pin(&hash);
        self.pinned.push(hash);
    }

    /// Hand the pins off to a longer-lived owner (a staged multipart upload):
    /// the chunks stay pinned and this guard's `Drop` becomes a no-op. Call only
    /// once the hashes are recorded somewhere that will unpin them later.
    fn disarm(&mut self) {
        self.pinned.clear();
    }
}

impl Drop for PinGuard<'_> {
    fn drop(&mut self) {
        self.chunks.unpin(&self.pinned);
    }
}

/// Releases a fixed set of in-flight chunk pins on drop — on *every* exit,
/// including an early `?` return or a panic. `complete_multipart` uses it: once
/// an upload is pulled out of the map its chunks are either about to become
/// reachable through the new pointer (success) or orphans (any failure), so the
/// in-flight pins must come off either way. Without it, a failed complete would
/// strand every pinned chunk — a client-triggerable leak.
struct ReleasePins<'a> {
    chunks: &'a barme_store::ChunkStore,
    hashes: Vec<Hash>,
}

impl Drop for ReleasePins<'_> {
    fn drop(&mut self) {
        self.chunks.unpin(&self.hashes);
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
