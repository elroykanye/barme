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
//!
//! Next: zstd encode/decode behind a Codec trait keyed by manifest.storage.codec.
