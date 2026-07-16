//! Content address. Everything in the store is named by one of these.
//!
//! Serializes as `blake3:<hex>` so manifests stay readable and the algorithm
//! travels with the value. That matters for the same reason manifests carry
//! their codec: if the hash function ever changes, old addresses still say
//! which one produced them.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

#[derive(Debug, thiserror::Error)]
pub enum HashError {
    #[error("missing algorithm prefix, expected `blake3:<hex>`")]
    MissingAlgo,
    #[error("unknown hash algorithm: {0}")]
    UnknownAlgo(String),
    #[error("invalid hex: {0}")]
    Hex(#[from] hex::FromHexError),
    #[error("wrong digest length: got {0} bytes, expected 32")]
    Length(usize),
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Hash([u8; 32]);

impl Hash {
    /// Hash a byte slice. This is the one place blake3 is called.
    pub fn of(bytes: &[u8]) -> Self {
        Hash(*blake3::hash(bytes).as_bytes())
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Bare hex digest, no algorithm prefix. Used for on-disk paths.
    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }
}

impl fmt::Display for Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "blake3:{}", hex::encode(self.0))
    }
}

impl fmt::Debug for Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self}")
    }
}

impl FromStr for Hash {
    type Err = HashError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (algo, hex_digest) = s.split_once(':').ok_or(HashError::MissingAlgo)?;
        if algo != "blake3" {
            return Err(HashError::UnknownAlgo(algo.to_string()));
        }
        let bytes = hex::decode(hex_digest)?;
        let arr: [u8; 32] = bytes
            .as_slice()
            .try_into()
            .map_err(|_| HashError::Length(bytes.len()))?;
        Ok(Hash(arr))
    }
}

impl Serialize for Hash {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for Hash {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_string() {
        let h = Hash::of(b"holiday.mp4");
        let s = h.to_string();
        assert!(s.starts_with("blake3:"));
        assert_eq!(h, s.parse().unwrap());
    }

    #[test]
    fn same_bytes_same_hash() {
        assert_eq!(Hash::of(b"abc"), Hash::of(b"abc"));
        assert_ne!(Hash::of(b"abc"), Hash::of(b"abd"));
    }

    #[test]
    fn rejects_bad_input() {
        assert!("deadbeef".parse::<Hash>().is_err()); // no algo
        assert!("sha256:deadbeef".parse::<Hash>().is_err()); // wrong algo
        assert!("blake3:zz".parse::<Hash>().is_err()); // bad hex
    }
}
