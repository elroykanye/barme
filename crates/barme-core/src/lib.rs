//! Core types shared across barme. Data and serialization only, no IO.

mod hash;
mod manifest;

pub use hash::{Hash, HashError};
pub use manifest::{
    Chunking, Fidelity, Manifest, Original, Quality, Route, Storage, MANIFEST_VERSION,
};
