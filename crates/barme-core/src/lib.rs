//! Core types shared across barme. Data and serialization only, no IO.

mod annotation;
mod bucket;
mod hash;
mod key;
mod manifest;
mod webhook;

pub use annotation::Annotation;
pub use bucket::BucketConfig;
pub use hash::{Hash, HashError};
pub use key::KeyRecord;
pub use manifest::{
    Chunking, Fidelity, Manifest, Original, Quality, Route, Storage, MANIFEST_VERSION,
};
pub use webhook::Webhook;
