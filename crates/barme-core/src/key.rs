//! An access credential. Stored server-side; the secret is kept in the clear
//! because SigV4 verification needs it to recompute signatures (same as AWS).

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyRecord {
    pub access_key: String,
    pub secret_key: String,
    /// A read-only key may only perform reads.
    #[serde(default)]
    pub read_only: bool,
    /// Pots this key is limited to. Empty means every pot (full owner).
    #[serde(default)]
    pub pots: Vec<String>,
    #[serde(default)]
    pub created_at: String,
}

impl KeyRecord {
    /// A full-access owner key: not read-only, not pot-scoped.
    pub fn is_owner(&self) -> bool {
        !self.read_only && self.pots.is_empty()
    }

    /// Whether this key may act on `pot`.
    pub fn scoped_to(&self, pot: &str) -> bool {
        self.pots.is_empty() || self.pots.iter().any(|p| p == pot)
    }
}
