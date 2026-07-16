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
//!
//! Next: the mark walk over a pointer snapshot.
