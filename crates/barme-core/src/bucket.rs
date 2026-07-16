//! Per-bucket configuration. Small for now (just visibility); this is also the
//! slot where per-bucket compression/fidelity policy will live later.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BucketConfig {
    /// When true, anyone may read this bucket's objects without credentials.
    /// Writes and deletes always require the owner regardless.
    #[serde(default)]
    pub public_read: bool,
}
