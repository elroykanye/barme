//! Per-pot configuration: visibility plus the storage policy new writes follow.
//! Any policy field left `None` falls back to the server default.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BucketConfig {
    /// When true, anyone may read this pot's objects without credentials.
    /// Writes and deletes always require the owner regardless.
    #[serde(default)]
    pub public_read: bool,

    /// Codec for new writes ("zstd" | "none"). None = server default.
    #[serde(default)]
    pub codec: Option<String>,
    /// zstd compression level. None = server default.
    #[serde(default)]
    pub zstd_level: Option<i32>,
    /// "exact" | "perceptual". None = exact.
    #[serde(default)]
    pub fidelity: Option<String>,
    /// Route image content types through the image path (recorded in the
    /// manifest; image codecs are a later tier).
    #[serde(default)]
    pub route_by_content_type: bool,

    /// Keep at most this many versions per key (0/None = unlimited).
    #[serde(default)]
    pub max_versions: Option<u32>,
    /// Expire objects older than this many days (0/None = never).
    #[serde(default)]
    pub expire_after_days: Option<u32>,
}
