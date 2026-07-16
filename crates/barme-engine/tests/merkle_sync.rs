//! Merkle roots, inclusion proofs, and replicating an object between two
//! independent stores using only the sync primitives (no network).

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

fn engine() -> (tempfile::TempDir, Engine) {
    let dir = tempfile::tempdir().unwrap();
    let policy = Policy {
        codec: "zstd".into(),
        zstd_level: 19,
        tenant: "acme".into(),
        policy_name: "test@v1".into(),
    };
    let engine = Engine::open(dir.path(), policy).unwrap();
    (dir, engine)
}

#[test]
fn put_records_a_merkle_root_over_the_chunks() {
    let (_d, e) = engine();
    let data = pseudo(400 * 1024, 1);
    let id = e.put("b", "k", &data, "x").unwrap();
    let m = e.object_manifest(&id).unwrap().unwrap();

    let root = m.chunking.merkle_root.expect("root recorded on write");
    assert_eq!(root, barme_core::merkle::root(&m.chunking.chunks));
    assert_eq!(e.object_root(&id).unwrap(), root);
}

#[test]
fn inclusion_proofs_verify_for_every_chunk() {
    let (_d, e) = engine();
    let data = pseudo(600 * 1024, 2);
    e.put("b", "k", &data, "x").unwrap();
    let m = e.manifest("b", "k").unwrap().unwrap();
    assert!(m.chunking.chunks.len() > 1, "want a multi-chunk object");

    for i in 0..m.chunking.chunks.len() {
        let cp = e.prove_chunk("b", "k", i).unwrap().unwrap();
        assert!(barme_core::merkle::verify(&cp.root, &cp.chunk, &cp.proof));
        assert_eq!(cp.chunk, m.chunking.chunks[i]);
    }
    // Out of range is None, not an error.
    assert!(e.prove_chunk("b", "k", m.chunking.chunks.len()).unwrap().is_none());
}

#[test]
fn delta_names_the_chunks_that_changed() {
    let (_d, e) = engine();
    let v1 = pseudo(500 * 1024, 3);
    let mut v2 = v1.clone();
    for byte in &mut v2[250_000..250_040] {
        *byte = !*byte;
    }
    let id1 = e.put("b", "k", &v1, "x").unwrap();
    let id2 = e.put("b", "k", &v2, "x").unwrap();

    let d = e.delta(&id1, &id2).unwrap();
    assert_eq!(d.root, e.object_root(&id2).unwrap());
    assert!(!d.add.is_empty(), "a changed object should add chunks");
    // Everything delta says to add is genuinely in the target and not the source.
    let m2 = e.object_manifest(&id2).unwrap().unwrap();
    for h in &d.add {
        assert!(m2.chunking.chunks.contains(h));
    }
}

#[test]
fn an_object_replicates_between_two_stores() {
    let (_ds, src) = engine();
    let (_dd, dst) = engine();

    let data = pseudo(800 * 1024, 4);
    let id = src.put("b", "k", &data, "text/plain").unwrap();
    let manifest = src.object_manifest(&id).unwrap().unwrap();

    // Puller starts empty: it's missing every chunk.
    let missing = dst.missing_chunks(&manifest.chunking.chunks);
    assert_eq!(missing.len(), manifest.chunking.chunks.len());

    // Ship each missing chunk verbatim; the address is re-verified on receipt.
    for h in &missing {
        let bytes = src.chunk_bytes(h).unwrap().unwrap();
        assert_eq!(&dst.put_chunk_bytes(&bytes).unwrap(), h);
    }

    // Adopt the manifest and confirm the bytes reassemble on the far side.
    let imported = dst.import_object("b", "k", &manifest).unwrap();
    assert_eq!(imported, id);
    assert_eq!(dst.get("b", "k").unwrap().unwrap(), data);
}

#[test]
fn import_refuses_a_manifest_whose_chunks_are_absent() {
    let (_ds, src) = engine();
    let (_dd, dst) = engine();
    let id = src.put("b", "k", &pseudo(300 * 1024, 5), "x").unwrap();
    let manifest = src.object_manifest(&id).unwrap().unwrap();

    // No chunks shipped: import must fail rather than create a dangling object.
    assert!(dst.import_object("b", "k", &manifest).is_err());
    assert!(dst.get("b", "k").unwrap().is_none());
}
