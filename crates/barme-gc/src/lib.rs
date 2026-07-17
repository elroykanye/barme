//! Mark-and-sweep garbage collection with a grace period.
//!
//! Chunks are shared, so a delete only moves a pointer; reclaiming chunks is
//! this crate's job. Reference counting was rejected: it keeps a second copy
//! of the truth that drifts under crashes and concurrency, and drift means
//! silent data loss. Mark-and-sweep re-derives reachability every pass, so it
//! self-corrects. The cost is CPU, which is an optimization problem.
//!
//!   MARK   from a snapshot of live pointers, walk to every reachable chunk
//!   SWEEP  condemn unreachable chunks (stamp a time), don't erase yet
//!   ERASE  delete chunks condemned longer than the grace window
//!
//! The dangerous case is a chunk unreferenced at MARK but reused by a
//! concurrent upload before ERASE. Guards: grace window, resurrection on
//! reference, never condemn a chunk younger than the window, snapshot roots.

use barme_core::Hash;
use barme_store::{Result, Store};
use std::collections::HashSet;
use std::time::Duration;

/// One collector bound to a store and a grace window. Clock time is passed in
/// per call, not read here, so passes are deterministic and testable.
pub struct Gc<'a> {
    store: &'a Store,
    grace: Duration,
}

/// What a sweep did, counted over the chunks it saw.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Sweep {
    /// Unreachable chunks newly condemned this pass.
    pub condemned: usize,
    /// Chunks past the grace window and erased this pass.
    pub erased: usize,
    /// Reachable chunks left in place.
    pub live: usize,
}

impl<'a> Gc<'a> {
    pub fn new(store: &'a Store, grace: Duration) -> Self {
        Gc { store, grace }
    }

    /// MARK. Snapshot every live pointer, walk each key's full history to its
    /// manifests, and collect the chunks they reference. Manifest visits are
    /// deduped so a chunk shared across versions is walked once. A history
    /// entry whose manifest is missing is skipped, not fatal: it can't make a
    /// present chunk unreachable, and the walk must not fail on a gap.
    pub fn mark(&self) -> Result<HashSet<Hash>> {
        // In-flight chunks come first: an upload that has written chunks but not
        // yet committed its pointer is invisible to the pointer walk below, so
        // without this a tight grace window could erase live upload data. The
        // pin set is the authoritative "don't touch, in use right now" signal.
        let mut reachable: HashSet<Hash> = self.store.chunks.pinned();
        let mut seen = HashSet::new();

        for bucket in self.store.pointers.buckets()? {
            for key in self.store.pointers.list(&bucket)? {
                for id in self.store.pointers.history(&bucket, &key)? {
                    if !seen.insert(id) {
                        continue; // manifest already walked
                    }
                    let Some(manifest) = self.store.manifests.get(&id)? else {
                        continue; // missing manifest, nothing to reach
                    };
                    reachable.extend(manifest.chunking.chunks);
                }
            }
        }
        Ok(reachable)
    }

    /// SWEEP + ERASE. Re-derive reachability, then reconcile every stored chunk
    /// against it and the condemned set:
    ///   - reachable        -> uncondemn if stamped (resurrection), keep
    ///   - condemned, aged  -> erase and drop the stamp
    ///   - unreachable, new -> stamp condemned_at = now
    /// A reachable chunk is never condemned, so a concurrent upload that reused
    /// a chunk before this pass clears the stamp instead of losing bytes.
    pub fn sweep(&self, now_secs: u64) -> Result<Sweep> {
        let reachable = self.mark()?;
        let mut condemned = self.store.chunks.load_condemned()?;
        let grace = self.grace.as_secs();
        let mut out = Sweep::default();

        for chunk in self.store.chunks.all()? {
            if reachable.contains(&chunk) {
                condemned.remove(&chunk); // resurrection
                out.live += 1;
                continue;
            }
            match condemned.get(&chunk).copied() {
                Some(at) if now_secs.saturating_sub(at) >= grace => {
                    self.store.chunks.delete(&chunk)?;
                    condemned.remove(&chunk);
                    out.erased += 1;
                }
                Some(_) => {} // condemned, still inside the grace window
                None => {
                    condemned.insert(chunk, now_secs);
                    out.condemned += 1;
                }
            }
        }

        self.store.chunks.save_condemned(&condemned)?;
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A chunk written by an in-flight upload — on disk, no pointer yet — must
    /// survive GC even with a zero grace window and repeated sweeps in the same
    /// instant. This is the exact data-loss race: without the pin, sweep 1
    /// condemns it and sweep 2 erases it before the upload commits, and the
    /// client still gets a success for an object missing its bytes.
    #[test]
    fn pinned_in_flight_chunk_survives_aggressive_gc() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let gc = Gc::new(&store, Duration::from_secs(0)); // zero grace: worst case

        let h = store.chunks.put(b"chunk from a live upload").unwrap();
        store.chunks.pin(&h); // engine pins the instant it's stored

        // Hammer GC while the "upload" is still in progress.
        for t in 0..5 {
            let s = gc.sweep(t).unwrap();
            assert_eq!(s.erased, 0, "erased a pinned in-flight chunk");
        }
        assert!(store.chunks.has(&h), "pinned chunk was reclaimed mid-upload");

        // Upload commits, pin released; chunk is now reachable by other means in
        // a real object, but even bare it is simply collectible garbage now.
        store.chunks.unpin(&[h]);
        gc.sweep(100).unwrap(); // condemn
        gc.sweep(100).unwrap(); // erase (grace 0)
        assert!(!store.chunks.has(&h), "unpinned orphan should be collectible");
    }

    /// The pin set must not wedge normal collection: an unpinned, unreferenced
    /// chunk (e.g. left by a crashed upload, whose pins died with the process) is
    /// still reclaimed on schedule.
    #[test]
    fn unpinned_orphan_is_still_collected() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let gc = Gc::new(&store, Duration::from_secs(10));

        let h = store.chunks.put(b"orphan from a crashed upload").unwrap();
        assert_eq!(gc.sweep(1000).unwrap().condemned, 1); // condemned at 1000
        assert_eq!(gc.sweep(1005).unwrap().erased, 0); // still inside grace
        assert!(store.chunks.has(&h));
        assert_eq!(gc.sweep(1011).unwrap().erased, 1); // past grace: gone
        assert!(!store.chunks.has(&h));
    }
}
