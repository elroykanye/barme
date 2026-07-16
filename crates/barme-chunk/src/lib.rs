//! Content-defined chunking (FastCDC).
//!
//! Splits bytes at boundaries chosen by the content, so a local edit only
//! disturbs the chunks it touches and downstream cut points re-sync. That
//! property is what makes dedup and cheap versioning work. Fixed-size chunking
//! would reshuffle every chunk after an edit and save nothing.

use barme_core::Hash;
use fastcdc::v2020::FastCDC;

/// Chunk size bounds, in bytes. FastCDC aims for `AVG` and stays within
/// `[MIN, MAX]`. Smaller means finer dedup but more chunks to track; these are
/// a reasonable middle ground and will likely become per-bucket config later.
pub const MIN_CHUNK: u32 = 16 * 1024;
pub const AVG_CHUNK: u32 = 64 * 1024;
pub const MAX_CHUNK: u32 = 256 * 1024;

/// One chunk: its content address and the bytes it owns.
#[derive(Debug, Clone)]
pub struct Chunk {
    pub hash: Hash,
    pub data: Vec<u8>,
}

/// Split `data` into content-defined chunks. Concatenating the chunks back
/// together reproduces `data` exactly.
pub fn chunk(data: &[u8]) -> Vec<Chunk> {
    FastCDC::new(data, MIN_CHUNK, AVG_CHUNK, MAX_CHUNK)
        .map(|c| {
            let slice = &data[c.offset..c.offset + c.length];
            Chunk {
                hash: Hash::of(slice),
                data: slice.to_vec(),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic pseudo-random bytes so chunk boundaries are varied but
    /// reproducible. All-zero input would chunk trivially and prove nothing.
    fn pseudo(len: usize, seed: u64) -> Vec<u8> {
        let mut s = seed;
        (0..len)
            .map(|_| {
                s = s
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                (s >> 33) as u8
            })
            .collect()
    }

    fn hashes(chunks: &[Chunk]) -> Vec<Hash> {
        chunks.iter().map(|c| c.hash).collect()
    }

    #[test]
    fn reassembles_to_original() {
        let data = pseudo(512 * 1024, 1);
        let rebuilt: Vec<u8> = chunk(&data).into_iter().flat_map(|c| c.data).collect();
        assert_eq!(rebuilt, data);
    }

    #[test]
    fn is_deterministic() {
        let data = pseudo(512 * 1024, 2);
        assert_eq!(hashes(&chunk(&data)), hashes(&chunk(&data)));
    }

    #[test]
    fn splits_into_several_chunks() {
        // 512 KiB at a 64 KiB average should land well above a handful,
        // otherwise the locality test below wouldn't be meaningful.
        let data = pseudo(512 * 1024, 3);
        assert!(chunk(&data).len() >= 4, "expected several chunks");
    }

    /// The property the whole design rests on: an in-place edit in the middle
    /// changes only the chunk(s) it lands in. Everything before is untouched
    /// and the boundaries after re-sync, so those chunks come back identical.
    #[test]
    fn edit_stays_local() {
        let v1 = pseudo(512 * 1024, 4);
        let mut v2 = v1.clone();
        for b in &mut v2[250_000..250_100] {
            *b = !*b;
        }

        let h1 = hashes(&chunk(&v1));
        let h2 = hashes(&chunk(&v2));

        // First and last chunks sit far from the edit and must survive.
        assert_eq!(h1.first(), h2.first(), "leading chunk should be unchanged");
        assert_eq!(h1.last(), h2.last(), "trailing chunk should re-sync");

        // A localized edit should touch very few chunks. Allow a small margin
        // for a boundary shifting around the edit point.
        let shared = h1.iter().filter(|h| h2.contains(h)).count();
        assert!(
            shared >= h1.len().saturating_sub(2),
            "expected almost all chunks shared, got {shared} of {}",
            h1.len()
        );
    }
}
