//! The per-object manifest: how an object was stored.
//!
//! This is the keystone. Reads are driven by the manifest, never by the
//! current server config. Config decides how a *new* object is written; the
//! manifest decides how an *existing* one is read back. That split is what
//! lets defaults change and codecs get added later without breaking old data.
//!
//! Mirrors the schema in docs/ARCHITECTURE.md.

use crate::Hash;
use serde::{Deserialize, Serialize};

pub const MANIFEST_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub manifest_version: u32,
    /// Content address of this manifest. Also the object_id.
    pub object_id: Hash,
    pub created_at: String,
    pub original: Original,
    pub storage: Storage,
    pub chunking: Chunking,
    pub quality: Quality,
    pub tenant: String,
    /// Which bucket policy was active at write time, e.g. "photos-bucket@v3".
    pub policy_snapshot: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Original {
    pub size_bytes: u64,
    /// Fingerprint of the true original bytes. On an exact read the output is
    /// re-hashed and checked against this.
    pub sha256: String,
    pub content_type: String,
}

/// Which storage path an object took. Whole-file image codecs and
/// content-defined chunking pull in opposite directions, so an object is
/// routed to one or the other, never both.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Route {
    /// Chunk with FastCDC, compress chunks, dedup per chunk.
    Blob,
    /// Treat the file as a whole, apply an image codec, dedup per file.
    Image,
}

/// Whether the stored form can reproduce the original bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Fidelity {
    /// Download equals the original, byte for byte.
    Exact,
    /// Looks identical, but is a different file.
    Perceptual,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Storage {
    pub route: Route,
    pub fidelity: Fidelity,
    pub codec: String,
    pub codec_params: serde_json::Value,
    pub stored_size_bytes: u64,
    /// The honest boolean: true for exact tiers, false for lossy ones.
    pub reconstructs_original: bool,
}

/// Present on the blob route. Empty (algo None) on the image route.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Chunking {
    pub algo: Option<String>,
    pub chunks: Vec<Hash>,
}

/// Present when fidelity is perceptual: records how faithful the result is,
/// so "how close to the original" is a stored fact rather than a guess.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Quality {
    /// e.g. "ssim", "vmaf", "butteraugli"
    pub metric: Option<String>,
    pub score: Option<f64>,
}
