//! Core types shared across barme. Data and serialization only, no IO.

mod bucket;
mod hash;
mod key;
mod manifest;

pub use bucket::BucketConfig;
pub use hash::{Hash, HashError};
pub use key::KeyRecord;
pub use manifest::{
    Chunking, Fidelity, Manifest, Original, Quality, Route, Storage, MANIFEST_VERSION,
};
