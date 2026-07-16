//! The identity codec. Stores bytes as-is. Useful as a floor when a bucket
//! opts out of compression, and as the simplest thing the read path can hit.

use crate::{Codec, Result};

pub struct Raw;

impl Codec for Raw {
    fn id(&self) -> &'static str {
        "none"
    }

    fn encode(&self, input: &[u8]) -> Result<Vec<u8>> {
        Ok(input.to_vec())
    }

    fn decode(&self, input: &[u8]) -> Result<Vec<u8>> {
        Ok(input.to_vec())
    }
}
