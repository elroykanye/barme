//! Adversarial concurrency: hammer the engine with racing writers and check the
//! invariants that must hold no matter how the threads interleave.

use barme_engine::{Engine, Policy};
use std::sync::Arc;
use std::thread;

fn engine() -> (tempfile::TempDir, Arc<Engine>) {
    let dir = tempfile::tempdir().unwrap();
    let policy = Policy {
        codec: "zstd".into(),
        zstd_level: 0,
        tenant: "acme".into(),
        policy_name: "test@v1".into(),
    };
    let engine = Engine::open(dir.path(), policy).unwrap();
    (dir, Arc::new(engine))
}

/// Every acknowledged write to one key must leave a version in history. The
/// store promises "every write keeps the previous version"; concurrent writers
/// must not silently drop one another's versions through a read-modify-write
/// race on the pointer file.
#[test]
fn concurrent_writes_to_one_key_keep_every_version() {
    let (_d, engine) = engine();
    const N: usize = 24;

    let handles: Vec<_> = (0..N)
        .map(|i| {
            let e = engine.clone();
            thread::spawn(move || {
                let body = format!("distinct version {i} - {:?}", vec![i as u8; 64]).into_bytes();
                e.put("pot", "key", &body, "text/plain").unwrap();
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }

    let hist = engine.history("pot", "key").unwrap();
    assert_eq!(
        hist.len(),
        N,
        "lost {} version(s) to a pointer write race",
        N - hist.len()
    );
    // The current object must still resolve and read back.
    let current = *engine.history("pot", "key").unwrap().last().unwrap();
    assert!(engine.read_object(&current).is_ok());
}

/// Racing puts and deletes on one key must never leave torn or corrupt state:
/// whatever the interleaving, the key ends either absent or resolvable to a
/// real object, and no operation panics.
#[test]
fn racing_put_and_delete_never_corrupt() {
    let (_d, engine) = engine();
    const ROUNDS: usize = 40;

    let handles: Vec<_> = (0..ROUNDS)
        .flat_map(|i| {
            let put = {
                let e = engine.clone();
                thread::spawn(move || {
                    let _ = e.put("pot", "contested", &vec![i as u8; 4096], "text/plain");
                })
            };
            let del = {
                let e = engine.clone();
                thread::spawn(move || {
                    let _ = e.delete("pot", "contested");
                })
            };
            [put, del]
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }

    // Final state must be coherent: history parses cleanly, and if a current
    // version exists it resolves to a readable object.
    let hist = engine.history("pot", "contested").unwrap();
    if let Some(current) = hist.last() {
        assert!(
            engine.read_object(current).is_ok(),
            "current version did not resolve after put/delete races"
        );
    }
}

/// Racing writers to *different* keys in the same pot must never corrupt each
/// other, and every key must end resolvable.
#[test]
fn concurrent_writes_to_distinct_keys_all_survive() {
    let (_d, engine) = engine();
    const N: usize = 48;

    let handles: Vec<_> = (0..N)
        .map(|i| {
            let e = engine.clone();
            thread::spawn(move || {
                let body = vec![(i % 251) as u8; 8 * 1024];
                e.put("pot", &format!("key-{i}"), &body, "application/octet-stream")
                    .unwrap();
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }

    let keys = engine.keys("pot").unwrap();
    assert_eq!(keys.len(), N, "a concurrent distinct-key write was lost");
    for i in 0..N {
        let key = format!("key-{i}");
        assert!(
            !engine.history("pot", &key).unwrap().is_empty(),
            "{key} did not resolve after concurrent writes"
        );
    }
}
