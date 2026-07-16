//! zstd: the exact-fidelity floor for the blob route. Fast, tunable, and the
//! bytes come back identical. Decode needs no level, so a default instance
//! reads anything zstd wrote regardless of the level it was written at.

use crate::{Codec, Result};

pub struct Zstd {
    level: i32,
}

impl Zstd {
    pub fn new(level: i32) -> Self {
        Zstd { level }
    }
}

impl Default for Zstd {
    fn default() -> Self {
        // zstd's own default. A good balance of ratio and speed; bucket policy
        // can dial it up for cold data later.
        Zstd::new(0)
    }
}

impl Codec for Zstd {
    fn id(&self) -> &'static str {
        "zstd"
    }

    fn encode(&self, input: &[u8]) -> Result<Vec<u8>> {
        Ok(zstd::encode_all(input, self.level)?)
    }

    fn decode(&self, input: &[u8]) -> Result<Vec<u8>> {
        Ok(zstd::decode_all(input)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        let z = Zstd::default();
        let data = b"round and round and round it goes";
        assert_eq!(z.decode(&z.encode(data).unwrap()).unwrap(), data);
    }

    #[test]
    fn shrinks_compressible_input() {
        let z = Zstd::new(19);
        let data = vec![b'a'; 100_000];
        let encoded = z.encode(&data).unwrap();
        assert!(
            encoded.len() < data.len() / 10,
            "expected heavy compression, got {} from {}",
            encoded.len(),
            data.len()
        );
        assert_eq!(z.decode(&encoded).unwrap(), data);
    }

    #[test]
    fn default_decodes_what_a_high_level_wrote() {
        let written = Zstd::new(19).encode(b"level independence").unwrap();
        assert_eq!(Zstd::default().decode(&written).unwrap(), b"level independence");
    }
}
