//! On-disk format versioning and the v1 schema freeze.
//!
//! These lock the format contract: a fresh dir is stamped, a dir from a newer
//! barme is refused rather than misread, an old un-stamped dir is adopted, and
//! the exact bytes of a v1 manifest are pinned by their content hash — so any
//! silent change to the manifest schema fails here and forces a conscious
//! version bump.

use barme_core::{
    Chunking, Fidelity, Hash, Manifest, Original, Quality, Route, Storage, MANIFEST_VERSION,
};
use barme_store::{Store, StoreError, FORMAT_VERSION};

#[test]
fn fresh_dir_is_stamped_with_current_format() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    assert_eq!(store.format_version, FORMAT_VERSION);
    assert!(dir.path().join("format.json").exists());
}

#[test]
fn a_newer_format_is_refused_not_misread() {
    let dir = tempfile::tempdir().unwrap();
    Store::open(dir.path()).unwrap(); // stamp v1
    // Hand-write a stamp from a hypothetical future barme.
    std::fs::write(
        dir.path().join("format.json"),
        br#"{"format_version": 999}"#,
    )
    .unwrap();
    match Store::open(dir.path()) {
        Err(StoreError::UnsupportedFormat { found, supported }) => {
            assert_eq!(found, 999);
            assert_eq!(supported, FORMAT_VERSION);
        }
        Err(e) => panic!("expected UnsupportedFormat, got a different error: {e}"),
        Ok(_) => panic!("expected UnsupportedFormat, but the store opened"),
    }
}

#[test]
fn an_unstamped_dir_is_adopted_and_its_data_survives() {
    let dir = tempfile::tempdir().unwrap();
    // Write real data, then remove the stamp to mimic a pre-stamp (pre-0.7) dir.
    let store = Store::open(dir.path()).unwrap();
    let h = store.chunks.put(b"data from before stamping existed").unwrap();
    drop(store);
    std::fs::remove_file(dir.path().join("format.json")).unwrap();

    // Reopening adopts it as v1 and the chunk is still readable.
    let store = Store::open(dir.path()).unwrap();
    assert_eq!(store.format_version, FORMAT_VERSION);
    assert!(dir.path().join("format.json").exists());
    assert_eq!(store.chunks.get(&h).unwrap().unwrap(), b"data from before stamping existed");
}

/// A manifest with every field fixed. Its `object_id` is the blake3 of its
/// canonical JSON, so this value freezes the exact v1 manifest schema: rename,
/// reorder, retype, or drop a field and the hash changes, failing the test.
fn frozen_manifest() -> Manifest {
    Manifest {
        manifest_version: MANIFEST_VERSION,
        object_id: Hash::of(b"ignored-on-store"),
        created_at: "2026-01-01T00:00:00Z".into(),
        original: Original {
            size_bytes: 2048,
            sha256: "0123456789abcdef".into(),
            content_type: "text/plain".into(),
        },
        storage: Storage {
            route: Route::Blob,
            fidelity: Fidelity::Exact,
            codec: "zstd".into(),
            codec_params: serde_json::json!({ "level": 0 }),
            stored_size_bytes: 1024,
            reconstructs_original: true,
        },
        chunking: Chunking {
            algo: Some("fastcdc".into()),
            chunks: vec![Hash::of(b"chunk-a"), Hash::of(b"chunk-b")],
            merkle_root: Some(Hash::of(b"root")),
        },
        quality: Quality::default(),
        tenant: "acme".into(),
        policy_snapshot: "default@v1".into(),
    }
}

#[test]
fn v1_manifest_schema_is_frozen() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let id = store.manifests.put(&frozen_manifest()).unwrap();
    // If this fails after a manifest change, that change is NOT backward
    // compatible: bump MANIFEST_VERSION and update this pin deliberately.
    assert_eq!(
        id.to_string(),
        "blake3:f1a48ba73f5bacdb5593f30b4a588fa29ae867c79fdc382fdb2fa5f76d0a5a3e",
        "v1 manifest schema changed; the object_id of a fixed manifest moved"
    );
    // And it round-trips.
    assert_eq!(store.manifests.get(&id).unwrap().unwrap().tenant, "acme");
}
