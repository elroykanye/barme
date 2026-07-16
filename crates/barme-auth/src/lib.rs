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

use std::collections::HashMap;

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

/// What a request wants to do. Read is GET/HEAD/list; the rest are owner-only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Read,
    Write,
    Delete,
    Admin,
}

/// The owner's declared credentials: access key -> secret key.
#[derive(Debug, Clone, Default)]
pub struct Credentials {
    keys: HashMap<String, String>,
}

impl Credentials {
    /// Read a single credential from BARME_ACCESS_KEY / BARME_SECRET_KEY.
    /// Returns None when either is unset, meaning auth is not configured.
    pub fn from_env() -> Option<Self> {
        let access = std::env::var("BARME_ACCESS_KEY").ok()?;
        let secret = std::env::var("BARME_SECRET_KEY").ok()?;
        if access.is_empty() || secret.is_empty() {
            return None;
        }
        let mut keys = HashMap::new();
        keys.insert(access, secret);
        Some(Credentials { keys })
    }

    /// Build credentials from a single access-key/secret pair.
    pub fn single(access_key: impl Into<String>, secret_key: impl Into<String>) -> Self {
        let mut keys = HashMap::new();
        keys.insert(access_key.into(), secret_key.into());
        Credentials { keys }
    }

    pub fn secret(&self, access_key: &str) -> Option<&str> {
        self.keys.get(access_key).map(String::as_str)
    }
}

/// The one authorization rule. `public_read` is the target bucket's flag.
pub fn authorize(principal: &Principal, action: Action, public_read: bool) -> bool {
    match principal {
        Principal::Owner(_) => true,
        Principal::Anonymous => action == Action::Read && public_read,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owner_can_do_anything() {
        let p = Principal::Owner("k".into());
        for action in [Action::Read, Action::Write, Action::Delete, Action::Admin] {
            assert!(authorize(&p, action, false));
        }
    }

    #[test]
    fn anonymous_reads_only_public_buckets() {
        let p = Principal::Anonymous;
        assert!(authorize(&p, Action::Read, true));
        assert!(!authorize(&p, Action::Read, false));
        assert!(!authorize(&p, Action::Write, true));
        assert!(!authorize(&p, Action::Delete, true));
    }
}
