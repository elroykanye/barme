//! Store an object the way the engine will, then read it back through the
//! pointer -> manifest -> chunks path and check it reassembles.

use barme_core::{
    Chunking, Fidelity, Hash, Manifest, Original, Quality, Route, Storage, MANIFEST_VERSION,
};
use barme_store::Store;

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
        let stored = store.chunks.put(c.data).unwrap();
        assert_eq!(stored, c.hash, "stored address must match the chunk's hash");
        chunk_hashes.push(stored);
    }

    let manifest = Manifest {
        manifest_version: MANIFEST_VERSION,
        object_id: Hash::of(b""), // set by the manifest store
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

/// Read path: pointer -> manifest -> chunks -> bytes.
fn read_object(store: &Store, bucket: &str, key: &str) -> Option<Vec<u8>> {
    let object_id = store.pointers.current(bucket, key).unwrap()?;
    let manifest = store.manifests.get(&object_id).unwrap().unwrap();
    let mut out = Vec::new();
    for h in &manifest.chunking.chunks {
        out.extend(store.chunks.get(h).unwrap().unwrap());
    }
    Some(out)
}

#[test]
fn write_then_read_reassembles() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();

    let data = pseudo(512 * 1024, 7);
    write_object(&store, "vids", "holiday.mp4", &data);

    assert_eq!(read_object(&store, "vids", "holiday.mp4").unwrap(), data);
}

#[test]
fn second_version_shares_chunks_and_keeps_the_first() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();

    let v1 = pseudo(512 * 1024, 8);
    let mut v2 = v1.clone();
    for b in &mut v2[250_000..250_100] {
        *b = !*b;
    }

    let id1 = write_object(&store, "vids", "holiday.mp4", &v1);
    let id2 = write_object(&store, "vids", "holiday.mp4", &v2);

    // New content -> new manifest, pointer moved, but v1 still resolvable.
    assert_ne!(id1, id2);
    assert_eq!(store.pointers.current("vids", "holiday.mp4").unwrap(), Some(id2));
    assert_eq!(read_object(&store, "vids", "holiday.mp4").unwrap(), v2);

    let m1 = store.manifests.get(&id1).unwrap().unwrap();
    let m2 = store.manifests.get(&id2).unwrap().unwrap();
    let shared = m1
        .chunking
        .chunks
        .iter()
        .filter(|h| m2.chunking.chunks.contains(h))
        .count();
    assert!(
        shared >= m1.chunking.chunks.len().saturating_sub(2),
        "the edit should have reused almost every chunk"
    );
}
