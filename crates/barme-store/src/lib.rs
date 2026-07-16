//! Content-addressed storage. All IO lives here.
//!
//! Three things to persist:
//!   - chunks:   keyed by hash, written once, never mutated
//!   - manifests: keyed by object_id (also a hash), immutable
//!   - pointers: bucket/key -> manifest hash. The only mutable state.
//!
//! Write-then-reference is a hard rule: chunks and the manifest are durable
//! before a pointer moves to them. GC leans on this to know a just-written
//! chunk is never garbage even before anything points at it.
//!
//! Next: a ChunkStore trait, then a filesystem implementation.
