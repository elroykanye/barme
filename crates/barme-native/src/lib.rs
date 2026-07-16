//! Native front door: the operations S3 has no vocabulary for.
//!
//!   GET  /objects/{key}/history    version graph, diffable
//!   GET  /content/{hash}           fetch any object or chunk by hash
//!   POST /sync                     send tree roots, get back missing subtrees
//!   GET  /objects/{key}/manifest   fidelity, codec, quality
//!   POST /search                   semantic retrieval
//!
//! Same engine as the S3 door, so an object written over S3 can be diffed and
//! introspected here.
//!
//! Next: axum router + the manifest and history endpoints.
