# barme 1.0.0

barme is 1.0. It makes three promises and keeps them:

1. **The on-disk format and API are frozen.** Anything written to a 1.x server
   reads on every later 1.x. Format changes ride a version stamp and a migration,
   never a silent break.
2. **You can trust it with data.** An acknowledged write survives a hard kill
   (fsync-durable, recovers on restart); concurrent writes and GC are safe under
   load; secrets are encrypted at rest; there's no standing default credential.
3. **It's operable.** Liveness + readiness probes, Prometheus `/metrics`, a
   documented and tested backup/restore story, and a Helm chart.

Everything is proven by harnesses that run in CI territory, not just asserted:
crash/kill durability, GC-under-load, security posture, multipart abuse, and a
copied-data-dir restore.

## In this release

Two delivery-correctness fixes closed before cutting 1.0:

- **`/cdn/{hash}` erasure caveat, documented (#6).** The immutable, cache-forever
  hash URL can't be revoked once bytes are in a cache, so deleting at the origin
  can't pull them back. This is now written down clearly — `/cdn` is for public,
  non-erasable content; serve erasable or personal data over the short-lived
  `/s/{pot}/{key}` share instead. Documented in USAGE, STABILITY, and the code.
- **Object hash surfaced on every write path (#7).** A new `X-Barme-Object-Id`
  response header carries the object's content id on single PUT, multipart
  complete, and HEAD — the reliable handle for a `/cdn` link, since an S3
  multipart ETag is a digest of part digests, not the object hash.

## The road here

- 0.4.x — durability (fsync, crash recovery) and concurrency/GC hardening
- 0.5.x — security (encrypted secrets, no default login, presign, CORS)
- 0.6.0 — S3 multipart upload
- 0.7.0 — on-disk format version + API freeze
- 0.8.0 — S3 bucket operations, Helm chart
- 0.9.0 — operability (backup/restore, readiness, metrics, name fuzzing)
- 1.0.0 — the delivery caveats above, and meaning the three promises

## Compatibility

Drop-in over 0.9.0. This is the compatibility baseline: 1.x won't break 1.0 data
or the stable API (see `docs/STABILITY.md`).

## Docker

```
docker run -p 7373:7373 -p 7374:7374 -p 7375:7375 -p 9000:9000 \
  -e BARME_MASTER_KEY=$(openssl rand -hex 32) \
  -v barme:/data elroykanye/barme:1.0.0
```

## Scope and what's next

1.0 is a single node you'd trust. Not in 1.0, by design: horizontal
distribution (the v2 headline — content-addressing makes replication cheap to add
later), encryption of object contents, and the experimental surfaces (semantic
search, sync, webhooks, image-codec transcoding) that stay marked as such.
