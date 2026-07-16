//! Content-defined chunking (FastCDC).
//!
//! Splits bytes at boundaries chosen by the content, so a local edit only
//! disturbs the chunks it touches and downstream cut points re-sync. That
//! property is what makes dedup and cheap versioning work. Fixed-size chunking
//! would reshuffle every chunk after an edit and save nothing.
//!
//! Next: wrap fastcdc, expose `chunk(bytes) -> Vec<(Hash, &[u8])>`.
