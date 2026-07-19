//! Authentication and authorization.
//!
//! Credentials are declared by the owner via environment. A request is either
//! the Owner (a valid credential) or Anonymous. Authorization is one rule:
//! owners do anything; anonymous callers may only read, and only from a bucket
//! marked public.
//!
//! The S3 door authenticates with AWS Signature V4 (see [`sigv4`]) so real S3
//! clients work unchanged. Other doors can check credentials more simply.

mod sigv4;

pub use sigv4::{verify_sigv4, SignedRequest};

use hmac::{Hmac, Mac};
use sha2::Sha256;
use std::collections::HashMap;

type HmacSha256 = Hmac<Sha256>;

/// Sign a `pot/key` for time-limited public delivery. Returns a hex HMAC-SHA256
/// over `"{pot}/{key}?exp={exp}"` keyed by the server secret. The CDN checks it
/// with [`verify_presign`] and serves the bytes even from a private pot.
pub fn presign(secret: &str, pot: &str, key: &str, exp_unix: u64) -> String {
    let msg = format!("{pot}/{key}?exp={exp_unix}");
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).expect("hmac accepts any key length");
    mac.update(msg.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

/// Verify a presigned signature and that it hasn't expired. Constant-time on the
/// signature comparison so a wrong guess leaks nothing through timing.
pub fn verify_presign(secret: &str, pot: &str, key: &str, exp: u64, sig: &str, now: u64) -> bool {
    if now > exp {
        return false;
    }
    let expected = presign(secret, pot, key, exp);
    secret_eq(&expected, sig)
}

/// Constant-time string equality, for comparing secrets and signatures. Runs in
/// time independent of *where* two equal-length inputs first differ, so an
/// attacker can't recover a secret byte by byte from response timing. Length is
/// allowed to short-circuit — it isn't the secret.
pub fn secret_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    a.len() == b.len()
        && a.iter()
            .zip(b)
            .fold(0u8, |acc, (x, y)| acc | (x ^ y))
            == 0
}

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("malformed Authorization header")]
    MalformedHeader,
    #[error("unknown access key")]
    UnknownKey,
    #[error("missing required header: {0}")]
    MissingHeader(&'static str),
    #[error("signature mismatch")]
    SignatureMismatch,
}

/// Who is making a request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Principal {
    Owner(String),
    Anonymous,
}

impl Principal {
    pub fn is_owner(&self) -> bool {
        matches!(self, Principal::Owner(_))
    }
}

/// What a request wants to do. Read is GET/HEAD/list; the rest need write access.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Read,
    Write,
    Delete,
    Admin,
}

use barme_core::KeyRecord;

/// The set of valid access keys, looked up during request verification.
#[derive(Debug, Clone, Default)]
pub struct Credentials {
    keys: HashMap<String, KeyRecord>,
}

impl Credentials {
    /// Build from the stored key records.
    pub fn from_records(records: impl IntoIterator<Item = KeyRecord>) -> Self {
        Credentials {
            keys: records
                .into_iter()
                .map(|r| (r.access_key.clone(), r))
                .collect(),
        }
    }

    /// A single full-owner credential (used in tests and simple setups).
    pub fn single(access_key: impl Into<String>, secret_key: impl Into<String>) -> Self {
        Self::from_records([KeyRecord {
            access_key: access_key.into(),
            secret_key: secret_key.into(),
            read_only: false,
            pots: vec![],
            created_at: String::new(),
        }])
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    pub fn secret(&self, access_key: &str) -> Option<&str> {
        self.keys.get(access_key).map(|r| r.secret_key.as_str())
    }

    pub fn record(&self, access_key: &str) -> Option<&KeyRecord> {
        self.keys.get(access_key)
    }
}

/// The authorization rule. `record` is the authenticated key (None = anonymous);
/// `pot` is the target pot; `public` is that pot's read flag.
pub fn authorize(record: Option<&KeyRecord>, action: Action, pot: &str, public: bool) -> bool {
    match record {
        Some(k) => {
            if k.read_only && action != Action::Read {
                return false;
            }
            k.scoped_to(pot)
        }
        None => action == Action::Read && public,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owner() -> KeyRecord {
        KeyRecord {
            access_key: "o".into(),
            secret_key: "s".into(),
            read_only: false,
            pots: vec![],
            created_at: String::new(),
        }
    }

    #[test]
    fn owner_can_do_anything() {
        let k = owner();
        for action in [Action::Read, Action::Write, Action::Delete, Action::Admin] {
            assert!(authorize(Some(&k), action, "any", false));
        }
    }

    #[test]
    fn read_only_key_cannot_write() {
        let mut k = owner();
        k.read_only = true;
        assert!(authorize(Some(&k), Action::Read, "any", false));
        assert!(!authorize(Some(&k), Action::Write, "any", false));
    }

    #[test]
    fn scoped_key_limited_to_its_pots() {
        let mut k = owner();
        k.pots = vec!["photos".into()];
        assert!(authorize(Some(&k), Action::Write, "photos", false));
        assert!(!authorize(Some(&k), Action::Write, "videos", false));
    }

    #[test]
    fn anonymous_reads_only_public() {
        assert!(authorize(None, Action::Read, "p", true));
        assert!(!authorize(None, Action::Read, "p", false));
        assert!(!authorize(None, Action::Write, "p", true));
    }

    #[test]
    fn presign_round_trips_and_expires() {
        let sig = presign("secret", "photos", "cat.jpg", 1000);
        assert!(verify_presign("secret", "photos", "cat.jpg", 1000, &sig, 999));
        // Expired.
        assert!(!verify_presign("secret", "photos", "cat.jpg", 1000, &sig, 1001));
        // Tampered path, wrong secret, or garbage signature all fail.
        assert!(!verify_presign("secret", "photos", "dog.jpg", 1000, &sig, 999));
        assert!(!verify_presign("other", "photos", "cat.jpg", 1000, &sig, 999));
        assert!(!verify_presign("secret", "photos", "cat.jpg", 1000, "deadbeef", 999));
    }
}
