# barme 0.4.1

Correctness under concurrency and garbage collection, found by an aggressive
stress campaign against 0.4.0. Three real defects fixed, all with regression
tests and adversarial harnesses.

## Fixes

- **Concurrent writes to one key no longer lose versions.** The pointer file is
  read-modify-write, and it had no serialization: 24 threads writing the same key
  landed **1** version in history, not 24 — 23 acknowledged writes silently
  dropped. Writes to a key now take a per-key commit lock (sharded, so different
  keys still run fully parallel and chunking stays off the lock). All 24 versions
  are now kept. This directly backs the store's "every write keeps the previous
  version" promise.
- **GC can no longer erase an in-flight upload's chunks.** A streaming upload
  writes all its chunks before committing its pointer, so those chunks are
  unreferenced for the whole upload. GC's own module doc promised a guard against
  reaping young chunks, but the guard wasn't implemented — with a tight grace
  window a sweep could condemn and erase a live upload's chunks, then the client
  got a success for an object missing its bytes. Uploads now **pin** each chunk
  the instant it's stored and unpin once the pointer commits; GC treats pinned
  chunks as reachable, so a sweep can't touch them no matter how aggressive the
  grace window. A crash still drops the pins, so a crashed upload's orphans are
  collected normally.
- **Malformed pot names return 400, not 500.** A pot name containing a slash or
  `..` (e.g. `/objects/..%2F..%2Fescape/k`) is rejected safely by the store, but
  the door surfaced it as a 500. It's a client error and now answers 400 on both
  the native and S3 doors. (The store already contained the input — nothing
  escaped; this is about the right status code.)
- **`barmed` reports the right version.** The daemon crate pinned its own
  `0.3.0` instead of inheriting the workspace version, so the 0.4.0 binary
  reported `0.3.0`. It now tracks the release version.

## Hardening (tests + harnesses)

- `scripts/crash-test.sh` gained a hot-GC mode (`GC_GRACE=`): `kill -9` mid-write
  *while GC actively sweeps and erases*. Verified: 8 crash cycles under a 2s grace
  with 1s sweeps, 62 objects, zero lost.
- `scripts/abuse-test.sh`: a battery of hostile HTTP inputs (overlong keys, path
  traversal in keys and pot names, control chars, empty/at-cap/over-cap bodies,
  bad auth, malformed JSON, huge headers) plus uploads under `grace=0` GC. The
  server survives every input and kept all ~290 objects intact.
- New unit/integration tests: in-flight pin survival under aggressive GC, orphan
  collection still works, and concurrent same-key / distinct-key write invariants.

## Upgrading

Drop-in. On-disk format unchanged; a 0.4.0 (or 0.3.x) data dir opens as-is.

## Docker

```
docker run -p 7373:7373 -p 7374:7374 -p 7375:7375 -p 9000:9000 -v barme:/data elroykanye/barme:0.4.1
```

## Known limits

- Alpha. Formats and on-disk layout may still change before v1.
- Secret keys are still stored in the clear and the default `barme/barme` login
  still works out of the box — both land in the next release. Don't expose it yet.
- A pot name plus key must encode to under 255 bytes (about 120 key bytes for a
  short pot).
- `barmed` binds IPv4 (`0.0.0.0`); on a dual-stack host reach it via `127.0.0.1`
  rather than `localhost`.
- Single node. Image codecs (JPEG XL, AVIF) are routed but not yet transcoding.
