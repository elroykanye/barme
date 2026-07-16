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
        let mut reachable = HashSet::new();
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
