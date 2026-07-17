//! The engine end to end: put bytes in, get the same bytes out, across codecs,
//! versions, and the compression path.

use barme_engine::{Engine, Policy};

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

fn engine(codec: &str) -> (tempfile::TempDir, Engine) {
    let dir = tempfile::tempdir().unwrap();
    let policy = Policy {
        codec: codec.into(),
        zstd_level: 19,
        tenant: "acme".into(),
        policy_name: "test@v1".into(),
    };
    let engine = Engine::open(dir.path(), policy).unwrap();
    (dir, engine)
}

#[test]
fn round_trips_under_both_codecs() {
    for codec in ["none", "zstd"] {
        let (_d, e) = engine(codec);
        let data = pseudo(300 * 1024, 1);
        e.put("b", "k", &data, "application/octet-stream").unwrap();
        assert_eq!(e.get("b", "k").unwrap().unwrap(), data, "codec {codec}");
    }
}

#[test]
fn missing_key_is_none() {
    let (_d, e) = engine("zstd");
    assert!(e.get("b", "ghost").unwrap().is_none());
    assert!(e.manifest("b", "ghost").unwrap().is_none());
}

#[test]
fn versions_accumulate_and_latest_wins() {
    let (_d, e) = engine("zstd");
    let v1 = pseudo(300 * 1024, 2);
    let mut v2 = v1.clone();
    for byte in &mut v2[150_000..150_050] {
        *byte = !*byte;
    }

    let id1 = e.put("b", "k", &v1, "x").unwrap();
    let id2 = e.put("b", "k", &v2, "x").unwrap();

    assert_ne!(id1, id2);
    assert_eq!(e.get("b", "k").unwrap().unwrap(), v2);
    assert_eq!(e.history("b", "k").unwrap(), vec![id1, id2]);
    // The older version is still readable directly by its id.
    assert_eq!(e.read_object(&id1).unwrap(), v1);
}

#[test]
fn rejects_empty_and_overlong_keys() {
    use barme_engine::{EngineError, MAX_NAME_BYTES};
    let (_d, e) = engine("zstd");

    // Empty key: rejected cleanly, not stored.
    let err = e.put("b", "", b"x", "x").unwrap_err();
    assert!(matches!(err, EngineError::InvalidKey(_)), "got {err:?}");

    // Largest key that still fits for a 1-byte pot: the encoded filename is
    // 2*pot + 2*key + len("_.json") = 2 + 2*key + 6, kept <= MAX_NAME_BYTES.
    let maxk = (MAX_NAME_BYTES - 8) / 2;

    // One byte over is rejected before touching the store...
    let toolong = "a".repeat(maxk + 1);
    let err = e.put("b", &toolong, b"x", "x").unwrap_err();
    assert!(matches!(err, EngineError::InvalidKey(_)), "got {err:?}");

    // ...but a key right at the limit is accepted and round-trips (this is the
    // case that must not blow up deep in the store with ENAMETOOLONG).
    let ok = "a".repeat(maxk);
    e.put("b", &ok, b"x", "x").unwrap();
    assert_eq!(e.get("b", &ok).unwrap().unwrap(), b"x");
}

#[test]
fn streamed_write_equals_buffered_write() {
    use std::io::Cursor;
    // Same bytes through both paths must chunk identically: same chunk set,
    // same digest, same size. (The object_id itself differs — it commits to
    // created_at — but the stored chunks and manifest content match.) Odd size
    // to catch any tail-handling difference between FastCDC and StreamCDC.
    for codec in ["none", "zstd"] {
        let (_d, e) = engine(codec);
        let data = pseudo(3 * 1024 * 1024 + 777, 9);
        e.put("b", "buffered", &data, "application/octet-stream").unwrap();
        e.put_stream(
            "b",
            "streamed",
            Cursor::new(data.clone()),
            "application/octet-stream",
            u64::MAX,
        )
        .unwrap();
        let mb = e.manifest("b", "buffered").unwrap().unwrap();
        let ms = e.manifest("b", "streamed").unwrap().unwrap();
        assert_eq!(mb.chunking.chunks, ms.chunking.chunks, "codec {codec}: chunk sets differ");
        assert_eq!(mb.chunking.merkle_root, ms.chunking.merkle_root, "codec {codec}: roots differ");
        assert_eq!(mb.original.sha256, ms.original.sha256, "codec {codec}: digests differ");
        assert_eq!(mb.original.size_bytes, ms.original.size_bytes, "codec {codec}: sizes differ");
        assert_eq!(e.get("b", "streamed").unwrap().unwrap(), data, "codec {codec}");
    }
}

#[test]
fn streamed_write_dedups_against_buffered() {
    use std::io::Cursor;
    let (_d, e) = engine("none");
    let data = pseudo(2 * 1024 * 1024, 11);
    e.put("b", "a", &data, "x").unwrap();
    let before = e.stats().unwrap().unique_chunks;
    e.put_stream("b", "b2", Cursor::new(data), "x", u64::MAX).unwrap();
    let after = e.stats().unwrap().unique_chunks;
    assert_eq!(before, after, "identical content streamed in should add no new chunks");
}

#[test]
fn streamed_write_enforces_size_cap() {
    use barme_engine::EngineError;
    use std::io::Cursor;
    let (_d, e) = engine("none");
    let data = pseudo(1_000_000, 13);
    let err = e
        .put_stream("b", "big", Cursor::new(data), "x", 100_000)
        .unwrap_err();
    assert!(
        matches!(err, EngineError::TooLarge { limit: 100_000 }),
        "got {err:?}"
    );
    // Nothing was committed: no pointer, so no object.
    assert!(e.get("b", "big").unwrap().is_none());
}

#[test]
fn compression_actually_shrinks_storage() {
    let (_d, e) = engine("zstd");
    let data = vec![b'a'; 500_000]; // very compressible
    e.put("b", "k", &data, "x").unwrap();

    let m = e.manifest("b", "k").unwrap().unwrap();
    assert_eq!(m.storage.codec, "zstd");
    assert!(
        m.storage.stored_size_bytes < m.original.size_bytes / 10,
        "expected heavy compression: stored {} vs original {}",
        m.storage.stored_size_bytes,
        m.original.size_bytes
    );
    // And it still reads back exactly.
    assert_eq!(e.get("b", "k").unwrap().unwrap(), data);
}

#[test]
fn manifest_records_how_it_was_written() {
    let (_d, e) = engine("zstd");
    e.put("photos", "cat.jpg", b"meow", "image/jpeg").unwrap();
    let m = e.manifest("photos", "cat.jpg").unwrap().unwrap();

    assert_eq!(m.original.content_type, "image/jpeg");
    assert_eq!(m.tenant, "acme");
    assert_eq!(m.policy_snapshot, "test@v1");
    assert!(m.storage.reconstructs_original);
    assert_eq!(m.codec_params_level(), Some(19));
}

// Small helper trait so the test can read the level out of codec_params.
trait CodecParamsExt {
    fn codec_params_level(&self) -> Option<i64>;
}
impl CodecParamsExt for barme_core::Manifest {
    fn codec_params_level(&self) -> Option<i64> {
        self.storage.codec_params.get("level")?.as_i64()
    }
}
