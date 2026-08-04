//! Engine-level object listing: prefix, delimiter grouping, and the pagination
//! S3 ListObjectsV2 is built on. The properties that matter to a mirroring tool
//! are that paging visits every key exactly once and that grouping matches what
//! a folder view expects.

use barme_engine::{Engine, Policy};

fn engine() -> (tempfile::TempDir, Engine) {
    let dir = tempfile::tempdir().unwrap();
    let e = Engine::open(dir.path(), Policy::default()).unwrap();
    (dir, e)
}

fn put_all(e: &Engine, bucket: &str, keys: &[&str]) {
    for k in keys {
        e.put(bucket, k, k.as_bytes(), "text/plain").unwrap();
    }
}

fn keys_of(page: &barme_engine::ObjectPage) -> Vec<String> {
    page.entries.iter().map(|o| o.key.clone()).collect()
}

/// Walk every page, the way a backup tool would, and return the keys in order.
fn drain(e: &Engine, bucket: &str, prefix: &str, delimiter: &str, max: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut after: Option<String> = None;
    loop {
        let page = e
            .list_objects(bucket, prefix, delimiter, after.as_deref(), max)
            .unwrap();
        out.extend(keys_of(&page));
        match page.next_after {
            Some(k) => after = Some(k),
            None => break,
        }
    }
    out
}

#[test]
fn lists_every_key_in_byte_order() {
    let (_d, e) = engine();
    put_all(&e, "pot", &["c.txt", "a.txt", "b.txt"]);
    let page = e.list_objects("pot", "", "", None, 1000).unwrap();
    assert_eq!(keys_of(&page), ["a.txt", "b.txt", "c.txt"]);
    assert!(page.next_after.is_none());
    assert!(page.common_prefixes.is_empty());
}

#[test]
fn an_empty_pot_lists_as_empty_not_as_an_error() {
    let (_d, e) = engine();
    e.create_bucket("fresh").unwrap();
    let page = e.list_objects("fresh", "", "", None, 1000).unwrap();
    assert!(page.entries.is_empty());
    assert!(page.next_after.is_none());
}

#[test]
fn entries_carry_size_and_the_object_id_head_reports() {
    let (_d, e) = engine();
    e.put("pot", "doc.txt", b"twelve bytes", "text/plain")
        .unwrap();
    let page = e.list_objects("pot", "", "", None, 1000).unwrap();
    let entry = &page.entries[0];
    assert_eq!(entry.size, 12);
    // Same handle a HEAD or a /cdn link would use, so a mirror can compare
    // without downloading.
    assert_eq!(
        entry.object_id,
        e.manifest("pot", "doc.txt").unwrap().unwrap().object_id
    );
    assert!(!entry.created_at.is_empty());
}

#[test]
fn prefix_keeps_only_matching_keys() {
    let (_d, e) = engine();
    put_all(&e, "pot", &["logs/a", "logs/b", "photos/c", "readme"]);
    let page = e.list_objects("pot", "logs/", "", None, 1000).unwrap();
    assert_eq!(keys_of(&page), ["logs/a", "logs/b"]);
}

#[test]
fn delimiter_collapses_folders_and_leaves_top_level_keys_alone() {
    let (_d, e) = engine();
    put_all(
        &e,
        "pot",
        &["logs/2026/a", "logs/2026/b", "photos/c", "readme"],
    );
    let page = e.list_objects("pot", "", "/", None, 1000).unwrap();
    // `readme` has no delimiter, so it stays an entry; the rest group.
    assert_eq!(keys_of(&page), ["readme"]);
    assert_eq!(page.common_prefixes, ["logs/", "photos/"]);
}

#[test]
fn delimiter_groups_below_a_prefix() {
    let (_d, e) = engine();
    put_all(
        &e,
        "pot",
        &["logs/2025/a", "logs/2026/a", "logs/2026/b", "logs/top"],
    );
    let page = e.list_objects("pot", "logs/", "/", None, 1000).unwrap();
    assert_eq!(keys_of(&page), ["logs/top"]);
    assert_eq!(page.common_prefixes, ["logs/2025/", "logs/2026/"]);
}

#[test]
fn paging_returns_every_key_exactly_once() {
    let (_d, e) = engine();
    let keys: Vec<String> = (0..25).map(|i| format!("k{i:03}")).collect();
    put_all(
        &e,
        "pot",
        &keys.iter().map(String::as_str).collect::<Vec<_>>(),
    );

    let mut expected = keys.clone();
    expected.sort();
    // A page size that doesn't divide the total, so the last page is partial.
    assert_eq!(drain(&e, "pot", "", "", 4), expected);
    // And a size of one, the worst case for an off-by-one in the cursor.
    assert_eq!(drain(&e, "pot", "", "", 1), expected);
}

#[test]
fn a_page_reports_truncation_only_while_keys_remain() {
    let (_d, e) = engine();
    put_all(&e, "pot", &["a", "b", "c"]);
    let first = e.list_objects("pot", "", "", None, 2).unwrap();
    assert_eq!(keys_of(&first), ["a", "b"]);
    assert_eq!(first.next_after.as_deref(), Some("b"));

    let second = e
        .list_objects("pot", "", "", first.next_after.as_deref(), 2)
        .unwrap();
    assert_eq!(keys_of(&second), ["c"]);
    // Exactly the remainder fit, so there is nothing to resume after.
    assert!(second.next_after.is_none());
}

#[test]
fn a_collapsed_prefix_is_not_repeated_across_pages() {
    let (_d, e) = engine();
    // Two folders, one of them fat enough to straddle several pages if its keys
    // were counted individually.
    put_all(
        &e,
        "pot",
        &["a/1", "a/2", "a/3", "a/4", "a/5", "b/1", "solo"],
    );

    let mut prefixes = Vec::new();
    let mut entries = Vec::new();
    let mut after: Option<String> = None;
    loop {
        let page = e.list_objects("pot", "", "/", after.as_deref(), 1).unwrap();
        prefixes.extend(page.common_prefixes.clone());
        entries.extend(keys_of(&page));
        match page.next_after {
            Some(k) => after = Some(k),
            None => break,
        }
    }
    // Each folder appears once, however many keys it holds and however the page
    // boundaries fall.
    assert_eq!(prefixes, ["a/", "b/"]);
    assert_eq!(entries, ["solo"]);
}

#[test]
fn a_group_counts_as_one_key_against_the_page_limit() {
    let (_d, e) = engine();
    put_all(&e, "pot", &["a/1", "a/2", "b/1", "c"]);
    let page = e.list_objects("pot", "", "/", None, 2).unwrap();
    // `a/` and `b/` fill the page; `c` waits for the next one.
    assert_eq!(page.common_prefixes, ["a/", "b/"]);
    assert!(page.entries.is_empty());
    let rest = e
        .list_objects("pot", "", "/", page.next_after.as_deref(), 2)
        .unwrap();
    assert_eq!(keys_of(&rest), ["c"]);
}

#[test]
fn start_after_resumes_strictly_past_the_named_key() {
    let (_d, e) = engine();
    put_all(&e, "pot", &["a", "b", "c"]);
    let page = e.list_objects("pot", "", "", Some("b"), 1000).unwrap();
    assert_eq!(keys_of(&page), ["c"]);
}

#[test]
fn a_deleted_key_drops_out_of_later_pages() {
    let (_d, e) = engine();
    put_all(&e, "pot", &["a", "b", "c"]);
    e.delete("pot", "b").unwrap();
    assert_eq!(drain(&e, "pot", "", "", 1000), ["a", "c"]);
}
