//! Semantic layer: a vector index keyed by content hash.
//!
//! Runs as a separate optional service, since vector search wants different
//! memory and hardware than byte storage. It is an index over the store, never
//! a source of truth: every embedding can be rebuilt from the stored bytes, so
//! losing it is a rebuild, not data loss.
//!
//! Built asynchronously after write, off the write path. Deduped by content
//! hash, so the same content is embedded once no matter how often it lands.
//! Versioned by model, so a better model can re-embed in the background.
//!
//! Next: define the index trait and the "understand(object_id)" job.
