//! User-facing metadata attached to an object by its `pot/key`, not part of the
//! content-addressed manifest. Free to change without moving any pointer.
//!
//! Tags double as the sink for auto-tagging: the understanding worker writes
//! whatever the configured embedder returns under `auto:<i>` keys, so proxied
//! labels sit beside hand-set ones.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Annotation {
    /// Arbitrary key/value labels. Auto-tags land under `auto:<i>` keys.
    #[serde(default)]
    pub tags: BTreeMap<String, String>,
    #[serde(default)]
    pub note: String,
    #[serde(default)]
    pub favorite: bool,
    /// While this rfc3339 timestamp is in the future, writes and deletes to the
    /// object are refused. Absent means never locked.
    #[serde(default)]
    pub locked_until: Option<String>,
}
