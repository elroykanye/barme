//! The engine. Ties chunking, codecs, storage, and GC into the read and write
//! paths, and owns version pointers.
//!
//! Write:  chunk -> dedup -> store new chunks -> build manifest -> move pointer
//! Read:   pointer -> manifest -> reassemble -> decompress per manifest -> verify
//!
//! Both front doors (S3 and native) call this and only this. No storage logic
//! lives in the doors, which is what keeps an object identical whichever way
//! it was written.
//!
//! Next: put(bucket, key, bytes, policy) and get(bucket, key).
