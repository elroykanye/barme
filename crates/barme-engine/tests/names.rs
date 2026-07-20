//! Property test for pot/key handling against the 255-byte filename bound. The
//! stores hex-encode `(pot, key)` into one filename, so odd names must never
//! panic — they either round-trip or are rejected cleanly with a known error.
//! Complements the hand-picked boundary cases in put_get.rs.

use barme_engine::{Engine, EngineError, Policy};
use proptest::prelude::*;

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn arbitrary_names_round_trip_or_reject_cleanly(
        // Any non-control characters, including multibyte unicode and slashes,
        // across a length range that straddles the 255-byte encoded limit.
        bucket in "\\PC{1,60}",
        key in "\\PC{0,180}",
    ) {
        let dir = tempfile::tempdir().unwrap();
        let e = Engine::open(dir.path(), Policy::default()).unwrap();
        let data = b"payload";

        // The one thing that must always hold: no panic, and a Result.
        match e.put(&bucket, &key, data, "application/octet-stream") {
            // Accepted -> it must read back byte-for-byte.
            Ok(_) => {
                let got = e.get(&bucket, &key).unwrap();
                prop_assert_eq!(got.as_deref(), Some(&data[..]));
            }
            // Rejected -> only for a bad/empty/too-long name or a bad pot name.
            Err(EngineError::InvalidKey(_)) | Err(EngineError::Store(_)) => {}
            Err(other) => prop_assert!(false, "unexpected error for ({bucket:?},{key:?}): {other:?}"),
        }
    }
}
