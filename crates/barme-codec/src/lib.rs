//! Compression, chosen per bucket and recorded per object.
//!
//! Tiers:
//!   1. zstd                        exact,      blob route floor
//!   2. JPEG XL lossless transcode  exact,      JPEGs ~20-30% smaller
//!   3. JPEG XL / AVIF lossy        perceptual, visually identical
//!   4. neural codecs               perceptual, not in scope yet
//!
//! Encode is chosen by policy on write. Decode is chosen by the object's
//! manifest, so the codec that wrote a byte is always the one that reads it.
//! That is why [`decoder_for`] takes the codec name straight from the manifest.

mod raw;
mod zstd_codec;

pub use raw::Raw;
pub use zstd_codec::Zstd;

#[derive(Debug, thiserror::Error)]
pub enum CodecError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("unknown codec: {0:?}")]
    Unknown(String),
}

pub type Result<T> = std::result::Result<T, CodecError>;

/// A reversible byte transform. Implementations are stateless; anything the
/// decoder needs to know rides in the compressed bytes or the manifest.
pub trait Codec {
    /// The name written into `manifest.storage.codec`.
    fn id(&self) -> &'static str;
    fn encode(&self, input: &[u8]) -> Result<Vec<u8>>;
    fn decode(&self, input: &[u8]) -> Result<Vec<u8>>;
}

/// Pick a decoder by the name recorded in a manifest. This is the read path:
/// it must handle every codec that was ever written, forever.
pub fn decoder_for(codec: &str) -> Result<Box<dyn Codec>> {
    match codec {
        "none" => Ok(Box::new(Raw)),
        "zstd" => Ok(Box::new(Zstd::default())),
        other => Err(CodecError::Unknown(other.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decoder_dispatches_by_name() {
        assert_eq!(decoder_for("none").unwrap().id(), "none");
        assert_eq!(decoder_for("zstd").unwrap().id(), "zstd");
        assert!(matches!(decoder_for("jxl"), Err(CodecError::Unknown(_))));
    }

    #[test]
    fn decoder_reverses_encoder_for_every_known_codec() {
        let payload = b"the pot keeps what the river brings, repeatedly repeatedly";
        for name in ["none", "zstd"] {
            let codec = decoder_for(name).unwrap();
            let round = codec.decode(&codec.encode(payload).unwrap()).unwrap();
            assert_eq!(round, payload, "codec {name} failed to round trip");
        }
    }
}
