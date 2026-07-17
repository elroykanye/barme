//! Scale probe: what does hammering a single key cost? The pointer file is
//! read-whole-then-rewrite per write, so this measures whether that O(n) rewrite
//! turns into a practical O(n^2) cliff, and whether the file grows without bound.
//!
//! Run with: cargo test -p barme-engine --test scale -- --nocapture --ignored

use barme_engine::{Engine, Policy};
use std::time::Instant;

fn engine() -> (tempfile::TempDir, Engine) {
    let dir = tempfile::tempdir().unwrap();
    let policy = Policy {
        codec: "zstd".into(),
        zstd_level: 0,
        tenant: "acme".into(),
        policy_name: "test@v1".into(),
    };
    let engine = Engine::open(dir.path(), policy).unwrap();
    (dir, engine)
}

#[test]
#[ignore = "scale probe, run explicitly with --ignored --nocapture"]
fn one_key_many_versions_cost() {
    let (dir, engine) = engine();
    const N: usize = 5000;
    let body = b"a small object rewritten over and over";

    let mut first_batch = std::time::Duration::ZERO;
    let mut last_batch = std::time::Duration::ZERO;
    let start = Instant::now();
    for i in 0..N {
        let t = Instant::now();
        engine.put("pot", "hot", body, "text/plain").unwrap();
        let dt = t.elapsed();
        if i < 100 {
            first_batch += dt;
        }
        if i >= N - 100 {
            last_batch += dt;
        }
    }
    let total = start.elapsed();

    let hist = engine.history("pot", "hot").unwrap();
    // Locate the pointer file to report its on-disk size.
    let ptr = dir
        .path()
        .join("pointers")
        .join("pot")
        .join(hex::encode("hot".as_bytes()));
    let ptr_bytes = std::fs::metadata(&ptr).map(|m| m.len()).unwrap_or(0);

    println!("--- one-key version explosion ---");
    println!("versions written: {N}, history len: {}", hist.len());
    println!("total time: {:?}", total);
    println!("first 100 writes avg: {:?}", first_batch / 100);
    println!("last 100 writes avg:  {:?}", last_batch / 100);
    println!(
        "slowdown last/first: {:.1}x",
        last_batch.as_secs_f64() / first_batch.as_secs_f64().max(1e-9)
    );
    println!("pointer file size: {} bytes ({} per version)", ptr_bytes, ptr_bytes as usize / N.max(1));

    assert_eq!(hist.len(), N, "versions were lost");
}
