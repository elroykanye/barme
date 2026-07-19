//! Adversarial multipart tests: the error paths, not the happy path. The point
//! is that a failed part or a botched complete must never strand pinned chunks —
//! a leaked pin keeps a chunk in memory's reachable set and off GC's radar
//! forever, so a client that keeps failing uploads could grow the store without
//! bound. `pinned_chunk_count()` is the leak gauge: zero once nothing is in
//! flight.

use barme_engine::{Engine, EngineError, Policy};
use std::io::{self, Read};

const MAX: u64 = 512 * 1024 * 1024;

fn engine() -> (tempfile::TempDir, Engine) {
    let dir = tempfile::tempdir().unwrap();
    let e = Engine::open(dir.path(), Policy::default()).unwrap();
    (dir, e)
}

fn bytes(n: usize, b: u8) -> Vec<u8> {
    vec![b; n]
}

/// Varied bytes so the content-defined chunker cuts several average-sized chunks
/// rather than one giant max-sized chunk (which uniform data produces). Needed
/// where the test must pin more than one chunk before an error.
fn varied(n: usize, seed: u64) -> Vec<u8> {
    let mut s = seed;
    (0..n)
        .map(|_| {
            s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (s >> 33) as u8
        })
        .collect()
}

/// Yields `remaining` bytes of filler, then fails — a client vanishing mid-part.
/// Sized so several chunks are cut (and pinned) before the read error hits.
struct FailingReader {
    remaining: usize,
}

impl Read for FailingReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.remaining == 0 {
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, "simulated disconnect"));
        }
        let n = buf.len().min(self.remaining).min(64 * 1024);
        buf[..n].fill(b'x');
        self.remaining -= n;
        Ok(n)
    }
}

#[test]
fn upload_part_disconnect_releases_pins() {
    let (_d, e) = engine();
    let up = e.create_multipart("v", "clip.bin", "application/octet-stream").unwrap();

    let res = e.upload_part(&up, 1, FailingReader { remaining: 300_000 }, MAX);
    assert!(
        matches!(res, Err(EngineError::Upload(_))),
        "expected an Upload error, got {res:?}"
    );
    // The path was actually exercised — chunks were stored before the failure...
    assert!(
        e.stats().unwrap().unique_chunks > 0,
        "no chunk was stored; the pin path wasn't exercised"
    );
    // ...yet none stayed pinned.
    assert_eq!(e.pinned_chunk_count(), 0, "upload_part leaked pins on disconnect");
}

#[test]
fn upload_part_oversize_releases_pins() {
    let (_d, e) = engine();
    let up = e.create_multipart("v", "clip.bin", "application/octet-stream").unwrap();

    // Varied 600 KB with a 300 KB cap: the chunker cuts several ~64 KB chunks, so
    // a few pin (each under the running cap) before one trips TooLarge — the pins
    // taken before the trip are exactly what must not leak.
    let res = e.upload_part(&up, 1, &varied(600_000, 7)[..], 300_000);
    assert!(matches!(res, Err(EngineError::TooLarge { .. })));
    assert!(
        e.stats().unwrap().unique_chunks > 0,
        "no chunk was stored; the pin path wasn't exercised"
    );
    assert_eq!(e.pinned_chunk_count(), 0, "upload_part leaked pins on oversize");
}

#[test]
fn complete_with_unknown_part_releases_pins_and_consumes_upload() {
    let (_d, e) = engine();
    let up = e.create_multipart("v", "clip.bin", "application/octet-stream").unwrap();
    e.upload_part(&up, 1, &bytes(200_000, b'a')[..], MAX).unwrap();
    assert!(e.pinned_chunk_count() > 0, "a staged part should be pinned");

    // Complete naming a part that was never uploaded.
    let res = e.complete_multipart(&up, &[999]);
    assert!(matches!(res, Err(EngineError::InvalidPart(999))));
    assert_eq!(e.pinned_chunk_count(), 0, "complete leaked pins on a bad part number");

    // The upload was consumed by the failed complete, so a retry sees it gone
    // (it can't be aborted either — the release guard is what prevents the leak).
    assert!(matches!(
        e.complete_multipart(&up, &[1]),
        Err(EngineError::NoSuchUpload(_))
    ));
}

#[test]
fn happy_path_completes_unpins_and_reassembles() {
    let (_d, e) = engine();
    let up = e.create_multipart("v", "clip.bin", "text/plain").unwrap();
    e.upload_part(&up, 1, &bytes(200_000, b'a')[..], MAX).unwrap();
    e.upload_part(&up, 2, &bytes(90_000, b'b')[..], MAX).unwrap();

    let id = e.complete_multipart(&up, &[1, 2]).unwrap();
    assert_eq!(e.pinned_chunk_count(), 0, "a completed upload left pins behind");

    let got = e.read_object(&id).unwrap();
    let mut want = bytes(200_000, b'a');
    want.extend_from_slice(&bytes(90_000, b'b'));
    assert_eq!(got, want);
}

#[test]
fn abort_after_a_part_unpins() {
    let (_d, e) = engine();
    let up = e.create_multipart("v", "clip.bin", "text/plain").unwrap();
    e.upload_part(&up, 1, &bytes(200_000, b'a')[..], MAX).unwrap();
    assert!(e.pinned_chunk_count() > 0);

    e.abort_multipart(&up).unwrap();
    assert_eq!(e.pinned_chunk_count(), 0, "abort left pins behind");
    // Idempotent.
    e.abort_multipart(&up).unwrap();
}

/// The wicked one: a long run of mixed failure modes must not accumulate pins.
/// With the leak, every disconnect and every bad complete would add chunks to
/// the pinned set permanently; here it must return to exactly zero.
#[test]
fn repeated_failures_do_not_accumulate_pins() {
    let (_d, e) = engine();
    for i in 0..80u32 {
        let up = e
            .create_multipart("v", &format!("k{i}"), "application/octet-stream")
            .unwrap();
        match i % 4 {
            0 => {
                // Disconnect mid-part, then the client aborts the husk.
                let _ = e.upload_part(&up, 1, FailingReader { remaining: 200_000 }, MAX);
                let _ = e.abort_multipart(&up);
            }
            1 => {
                // A real part, then complete naming a part that doesn't exist.
                e.upload_part(&up, 1, &bytes(200_000, b'a')[..], MAX).unwrap();
                let _ = e.complete_multipart(&up, &[42]);
            }
            2 => {
                // A real part, then an explicit abort.
                e.upload_part(&up, 1, &bytes(150_000, b'c')[..], MAX).unwrap();
                e.abort_multipart(&up).unwrap();
            }
            _ => {
                // A clean completion.
                e.upload_part(&up, 1, &bytes(120_000, b'd')[..], MAX).unwrap();
                e.complete_multipart(&up, &[1]).unwrap();
            }
        }
        // Nothing is left in flight at the end of any iteration.
        assert_eq!(
            e.pinned_chunk_count(),
            0,
            "pins accumulated after iteration {i} (mode {})",
            i % 4
        );
    }
}
