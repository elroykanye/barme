//! Engine-level pot (bucket) operations: explicit create, existence, listing,
//! and the atomic empty-only delete that S3 DeleteBucket relies on.

use barme_engine::{Engine, Policy};

fn engine() -> (tempfile::TempDir, Engine) {
    let dir = tempfile::tempdir().unwrap();
    let e = Engine::open(dir.path(), Policy::default()).unwrap();
    (dir, e)
}

#[test]
fn create_is_idempotent_and_visible_while_empty() {
    let (_d, e) = engine();
    assert!(!e.bucket_exists("reports").unwrap());
    e.create_bucket("reports").unwrap();
    e.create_bucket("reports").unwrap(); // idempotent — no error on repeat
    assert!(e.bucket_exists("reports").unwrap());
    assert!(e.list_buckets().unwrap().contains(&"reports".to_string()));
}

#[test]
fn a_written_pot_exists_and_lists_without_explicit_create() {
    let (_d, e) = engine();
    e.put("implied", "k", b"hi", "text/plain").unwrap();
    assert!(e.bucket_exists("implied").unwrap());
    assert!(e.list_buckets().unwrap().contains(&"implied".to_string()));
}

#[test]
fn delete_if_empty_refuses_a_nonempty_pot_and_keeps_its_data() {
    let (_d, e) = engine();
    e.put("full", "doc", b"data", "text/plain").unwrap();
    // Refused (false = deleted nothing), so the caller answers 409.
    assert!(!e.delete_bucket_if_empty("full").unwrap());
    // The object is untouched — this is the durability guarantee the atomic
    // check protects: a refused delete never wipes a pointer.
    assert_eq!(e.get("full", "doc").unwrap().unwrap(), b"data");
}

#[test]
fn delete_if_empty_removes_an_empty_pot() {
    let (_d, e) = engine();
    e.create_bucket("temp").unwrap();
    assert!(e.bucket_exists("temp").unwrap());
    assert!(e.delete_bucket_if_empty("temp").unwrap());
    assert!(!e.bucket_exists("temp").unwrap());
}
