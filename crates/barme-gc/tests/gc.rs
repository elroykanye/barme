//! GC behaviour end to end: build objects through the store the way the engine
//! will, then mark and sweep and check what survives.

use barme_core::{
    Chunking, Fidelity, Hash, Manifest, Original, Quality, Route, Storage, MANIFEST_VERSION,
};
use barme_gc::Gc;
use barme_store::Store;
use std::time::Duration;

const GRACE: Duration = Duration::from_secs(3600);

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

/// Write path: chunk, store chunks, build manifest, move pointer.
fn write_object(store: &Store, bucket: &str, key: &str, data: &[u8]) -> Hash {
    let mut chunk_hashes = Vec::new();
    for c in barme_chunk::chunk(data) {
        chunk_hashes.push(store.chunks.put(&c.data).unwrap());
    }
    let manifest = Manifest {
        manifest_version: MANIFEST_VERSION,
        object_id: Hash::of(b""),
        created_at: "2026-07-16T00:00:00Z".into(),
        original: Original {
            size_bytes: data.len() as u64,
            sha256: "unused-in-test".into(),
            content_type: "application/octet-stream".into(),
        },
        storage: Storage {
            route: Route::Blob,
            fidelity: Fidelity::Exact,
            codec: "none".into(),
            codec_params: serde_json::json!({}),
            stored_size_bytes: data.len() as u64,
            reconstructs_original: true,
        },
        chunking: Chunking {
            algo: Some("fastcdc".into()),
            chunks: chunk_hashes,
            merkle_root: None,
        },
        quality: Quality::default(),
        tenant: "acme".into(),
        policy_snapshot: "default@v1".into(),
    };
    let object_id = store.manifests.put(&manifest).unwrap();
    store.pointers.set(bucket, key, &object_id).unwrap();
    object_id
}

fn chunks_of(store: &Store, object_id: &Hash) -> Vec<Hash> {
    store.manifests.get(object_id).unwrap().unwrap().chunking.chunks
}

#[test]
fn live_chunk_is_never_condemned() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let id = write_object(&store, "vids", "holiday.mp4", &pseudo(512 * 1024, 7));

    let gc = Gc::new(&store, GRACE);
    let sweep = gc.sweep(1_000).unwrap();

    assert_eq!(sweep.condemned, 0);
    assert_eq!(sweep.erased, 0);
    for h in chunks_of(&store, &id) {
        assert!(store.chunks.has(&h), "live chunk must stay on disk");
    }
}

#[test]
fn deleted_key_condemns_then_erases_past_grace() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let id = write_object(&store, "vids", "holiday.mp4", &pseudo(512 * 1024, 8));
    let chunks = chunks_of(&store, &id);

    store.pointers.remove("vids", "holiday.mp4").unwrap();

    let gc = Gc::new(&store, GRACE);

    // First pass condemns, erases nothing; chunks are still readable.
    let first = gc.sweep(1_000).unwrap();
    assert_eq!(first.erased, 0);
    assert!(first.condemned >= 1);
    for h in &chunks {
        assert!(store.chunks.has(h), "condemned chunk is not erased yet");
    }

    // A pass past the grace window erases the condemned chunks.
    let second = gc.sweep(1_000 + GRACE.as_secs()).unwrap();
    assert_eq!(second.erased, chunks.len());
    for h in &chunks {
        assert!(!store.chunks.has(h), "chunk erased after grace window");
    }
}

#[test]
fn chunk_shared_by_two_keys_survives_deleting_one() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let data = pseudo(512 * 1024, 9);
    let id = write_object(&store, "vids", "a.mp4", &data);
    write_object(&store, "vids", "b.mp4", &data); // same bytes, shared chunks
    let chunks = chunks_of(&store, &id);

    store.pointers.remove("vids", "a.mp4").unwrap();

    let gc = Gc::new(&store, GRACE);
    let sweep = gc.sweep(1_000).unwrap();

    // b.mp4 still references every chunk, so nothing is condemned.
    assert_eq!(sweep.condemned, 0);
    for h in &chunks {
        assert!(store.chunks.has(h), "chunk kept alive by the surviving key");
    }
}

#[test]
fn condemned_chunk_is_resurrected_before_erase() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let data = pseudo(512 * 1024, 10);
    let id = write_object(&store, "vids", "a.mp4", &data);
    let chunks = chunks_of(&store, &id);

    store.pointers.remove("vids", "a.mp4").unwrap();

    let gc = Gc::new(&store, GRACE);

    // Sweep 1 condemns the now-unreferenced chunks.
    let first = gc.sweep(1_000).unwrap();
    assert!(first.condemned >= 1);

    // Before the grace window closes, the same bytes are referenced again.
    write_object(&store, "vids", "b.mp4", &data);

    // Sweep 2, well past the window: reachable again, so uncondemned, not erased.
    let second = gc.sweep(1_000 + GRACE.as_secs() + 1).unwrap();
    assert_eq!(second.erased, 0);
    for h in &chunks {
        assert!(store.chunks.has(h), "resurrected chunk survives");
    }
}
