# barme 0.4.2

More hardening from the same stress campaign that produced 0.4.1. Three
resilience and concurrency fixes, each with a regression test.

## Fixes

- **A corrupt condemned-set file no longer wedges GC.** The `.condemned` file
  (chunk -> when-condemned) was deserialized with the error propagated, so a
  single bad byte in it would fail every future sweep — GC would stop forever and
  the disk would fill without bound. It now heals to empty: the set is disposable
  derived state (mark re-derives reachability every pass; the stamps only gate the
  grace window), so the worst case is a one-grace-period delay in reclaiming
  chunks. This matches how the rest of the collector re-derives its own truth.
- **Concurrent delete and put on one key can't lose the delete.** `delete` now
  takes the same per-key commit lock that writes do. Without it a delete could
  interleave with a put's read-modify-write and be silently undone (the put reads
  the history, the delete removes the file, the put rewrites it and resurrects the
  key). Serializing makes it clean last-writer-wins.
- **A corrupt pointer line no longer orphans the whole key.** The pointer file is
  the one mutable, non-content-addressed piece of state, so it's the one place a
  disk bit-flip isn't caught by an address check. A single unparseable line used
  to fail the entire read, taking the key's intact versions and rollback down with
  it. Corrupt lines are now skipped, so the surviving versions stay readable. (A
  corrupt *current* line resolves to the previous version rather than erroring —
  availability over a hard failure, consistent with the rest of the store.)

## Under the campaign, holding up

- A version-explosion probe (5000 writes to one key) showed **flat ~per-write
  cost, no O(n²) cliff** — the pointer rewrite is dominated by the per-write fsync,
  not by history length. See known limits for the one caveat at extreme counts.
- New adversarial tests: racing put/delete on one key never corrupts state;
  corrupt condemned set and corrupt pointer line both recover.

## Upgrading

Drop-in. On-disk format unchanged; a 0.4.x (or 0.3.x) data dir opens as-is.

## Docker

```
docker run -p 7373:7373 -p 7374:7374 -p 7375:7375 -p 9000:9000 -v barme:/data elroykanye/barme:0.4.2
```

## Known limits

- Alpha. Formats and on-disk layout may still change before v1.
- Secret keys are still stored in the clear and the default `barme/barme` login
  still works out of the box — both land in the next release. Don't expose it yet.
- A single key's version history has no cap by default (set `max_versions` per pot
  to bound it). The pointer file grows linearly with versions and is rewritten per
  write, so a key written millions of times becomes a large, slow file. A default
  cap is planned.
- A pot name plus key must encode to under 255 bytes (about 120 key bytes for a
  short pot).
- `barmed` binds IPv4 (`0.0.0.0`); on a dual-stack host reach it via `127.0.0.1`
  rather than `localhost`.
- Single node. Image codecs (JPEG XL, AVIF) are routed but not yet transcoding.
