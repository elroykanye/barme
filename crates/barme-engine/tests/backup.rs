//! The data directory is a coherent backup target: a copy of it, opened
//! elsewhere, restores every object byte-for-byte. This is the "back it up and
//! recover it" v1 promise, proven rather than asserted.

use barme_engine::{Engine, Policy};
use std::fs;
use std::path::Path;

/// Recursively copy a directory tree — a stand-in for `cp -r` / a volume
/// snapshot of a quiesced data dir.
fn copy_dir(src: &Path, dst: &Path) {
    fs::create_dir_all(dst).unwrap();
    for entry in fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let (from, to) = (entry.path(), dst.join(entry.file_name()));
        if from.is_dir() {
            copy_dir(&from, &to);
        } else {
            fs::copy(&from, &to).unwrap();
        }
    }
}

#[test]
fn a_copied_data_dir_restores_clean() {
    let src = tempfile::tempdir().unwrap();

    // Write a spread of objects across pots, sizes, and a second version.
    {
        let e = Engine::open(src.path(), Policy::default()).unwrap();
        e.put("photos", "a.bin", &vec![1u8; 5_000], "application/octet-stream")
            .unwrap();
        e.put("photos", "b.bin", &vec![2u8; 200_000], "application/octet-stream")
            .unwrap();
        e.put("docs", "note.txt", b"hello backup", "text/plain").unwrap();
        // Overwrite a.bin so the pointer file carries history — the restore must
        // return the latest version.
        e.put("photos", "a.bin", &vec![9u8; 5_000], "application/octet-stream")
            .unwrap();
    } // engine dropped: the dir is quiesced, like a stopped process

    // Back up: copy the whole data dir elsewhere.
    let dst = tempfile::tempdir().unwrap();
    copy_dir(src.path(), dst.path());

    // Restore: open the copy as a fresh store. Every object reads back exactly,
    // integrity holds, and the format stamp came along.
    let e = Engine::open(dst.path(), Policy::default()).unwrap();
    assert_eq!(e.get("photos", "a.bin").unwrap().unwrap(), vec![9u8; 5_000]);
    assert_eq!(e.get("photos", "b.bin").unwrap().unwrap(), vec![2u8; 200_000]);
    assert_eq!(e.get("docs", "note.txt").unwrap().unwrap(), b"hello backup");
    assert!(e.verify("photos", "b.bin").unwrap());
    assert!(e.verify("photos", "a.bin").unwrap());
    assert_eq!(e.format_version(), 1);

    // History survived too: the restored copy still knows both versions of a.bin.
    assert_eq!(e.keys("photos").unwrap().len(), 2);
}
