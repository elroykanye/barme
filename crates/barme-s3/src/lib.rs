//! S3-compatible front door.
//!
//! bucket/key/object maps almost directly onto bucket/pointer/manifest:
//!   PUT    -> engine write path
//!   GET    -> engine read path
//!   DELETE -> move/clear pointer (chunks reclaimed later by GC, never inline)
//!   HEAD   -> manifest lookup; etag is the content hash
//!   List   -> list pointers
//!   multipart, presigned URLs
//!
//! Scope: the parts real clients use first. The long tail of bucket
//! sub-resources (ACLs, lifecycle, policies) comes later.
//!
//! Next: axum router + PUT/GET/HEAD/DELETE over the engine.
