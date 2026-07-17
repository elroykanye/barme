//! Content-defined chunking (FastCDC).
//!
//! Splits bytes at boundaries chosen by the content, so a local edit only
//! disturbs the chunks it touches and downstream cut points re-sync. That
//! property is what makes dedup and cheap versioning work. Fixed-size chunking
//! would reshuffle every chunk after an edit and save nothing.

use barme_core::Hash;
use fastcdc::v2020::{FastCDC, StreamCDC};
use std::io::Read;

/// Chunk size bounds, in bytes. FastCDC aims for `AVG` and stays within
/// `[MIN, MAX]`. Smaller means finer dedup but more chunks to track; these are
/// a reasonable middle ground and will likely become per-bucket config later.
pub const MIN_CHUNK: u32 = 16 * 1024;
pub const AVG_CHUNK: u32 = 64 * 1024;
pub const MAX_CHUNK: u32 = 256 * 1024;

/// One chunk: its content address and a borrow of the bytes it covers. The
/// slice points into the caller's buffer — no copy is made.
#[derive(Debug, Clone, Copy)]
pub struct Chunk<'a> {
    pub hash: Hash,
    pub data: &'a [u8],
}

/// Split `data` into content-defined chunks, lazily. Concatenating the chunks
/// back together reproduces `data` exactly.
///
/// This yields borrowed slices, so the whole input is never duplicated: the
/// caller holds one copy of the bytes (the input), and each chunk is a view
/// into it. An earlier version collected owned `Vec<u8>` per chunk, which held
/// a second full copy of the object in memory during a write — doubling the
/// footprint of every upload.
pub fn chunk(data: &[u8]) -> impl Iterator<Item = Chunk<'_>> {
    FastCDC::new(data, MIN_CHUNK, AVG_CHUNK, MAX_CHUNK).map(move |c| {
        let slice = &data[c.offset..c.offset + c.length];
        Chunk {
            hash: Hash::of(slice),
            data: slice,
        }
    })
}

/// The streaming counterpart to [`chunk`]: split bytes read from `source` into
/// content-defined chunks without ever holding the whole input in memory. The
/// chunker buffers only a bounded window (a few times [`MAX_CHUNK`]), so memory
/// stays flat no matter how large the object is.
///
/// Each item owns its chunk's bytes (there's no backing buffer to borrow from).
/// For the same input, the boundaries — and therefore the chunk hashes — match
/// [`chunk`] exactly, so an object streamed in dedups against the same object
/// written buffered.
pub fn chunk_stream<R: Read>(source: R) -> impl Iterator<Item = std::io::Result<(Hash, Vec<u8>)>> {
    StreamCDC::new(source, MIN_CHUNK, AVG_CHUNK, MAX_CHUNK).map(|res| match res {
        Ok(cd) => Ok((Hash::of(&cd.data), cd.data)),
        Err(e) => Err(std::io::Error::new(std::io::ErrorKind::Other, e)),
    })
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

    fn hashes(data: &[u8]) -> Vec<Hash> {
        chunk(data).map(|c| c.hash).collect()
    }

    #[test]
    fn reassembles_to_original() {
        let data = pseudo(512 * 1024, 1);
        let rebuilt: Vec<u8> = chunk(&data).flat_map(|c| c.data.to_vec()).collect();
        assert_eq!(rebuilt, data);
    }

    #[test]
    fn is_deterministic() {
        let data = pseudo(512 * 1024, 2);
        assert_eq!(hashes(&data), hashes(&data));
    }

    #[test]
    fn stream_matches_buffered() {
        // The whole point of streaming: identical boundaries and hashes to the
        // in-memory path, so dedup lines up regardless of how an object arrived.
        for len in [0usize, 1, 1000, 512 * 1024, 3 * 1024 * 1024] {
            let data = pseudo(len, 7);
            let buffered: Vec<Hash> = chunk(&data).map(|c| c.hash).collect();
            let streamed: Vec<Hash> = chunk_stream(&data[..])
                .map(|r| r.unwrap().0)
                .collect();
            assert_eq!(buffered, streamed, "mismatch at len {len}");

            // And the streamed chunks reassemble to the original bytes.
            let rebuilt: Vec<u8> = chunk_stream(&data[..])
                .flat_map(|r| r.unwrap().1)
                .collect();
            assert_eq!(rebuilt, data, "reassembly mismatch at len {len}");
        }
    }

    #[test]
    fn splits_into_several_chunks() {
        // 512 KiB at a 64 KiB average should land well above a handful,
        // otherwise the locality test below wouldn't be meaningful.
        let data = pseudo(512 * 1024, 3);
        assert!(chunk(&data).count() >= 4, "expected several chunks");
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

        let h1 = hashes(&v1);
        let h2 = hashes(&v2);

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
