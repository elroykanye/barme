# barme 0.7.0

Format and API freeze — the groundwork that lets barme evolve without orphaning
data or breaking clients, and the last big block before a v1 that means something.

## What changed

- **On-disk format is versioned.** A `format.json` at the data root stamps the
  layout version. On open, a directory written by a *newer* barme is refused
  (rather than risk misreading a layout it doesn't know), an older one runs its
  migration and rewrites the stamp (the hook is in place; no steps needed yet),
  and a pre-0.7 directory is adopted as v1 with its data intact. The resolved
  version is logged at startup.
- **Manifests enforce their version.** Every object already carried a
  `manifest_version`; reads now refuse a manifest newer than this build
  understands, per object, before trusting its fields.
- **The v1 manifest schema is pinned.** A test hashes a fully-fixed manifest and
  asserts its content address, so any silent change to the schema fails CI and
  forces a conscious version bump.
- **The API surface is frozen and documented.** `docs/STABILITY.md` states the
  compatibility promise and lists the stable surface (objects & versions, pots,
  keys, the S3 and CDN doors, presign, ops) versus the experimental surface
  (semantic search, sync, webhooks, image codecs), which is now marked as such in
  the live `/docs`.

## Compatibility

Drop-in. An existing data directory is stamped `format v1` on first open under
0.7.0 and keeps working unchanged. From here on, within a major version, data and
the stable API don't break — see `docs/STABILITY.md`.

## Docker

```
docker run -p 7373:7373 -p 7374:7374 -p 7375:7375 -p 9000:9000 \
  -e BARME_MASTER_KEY=$(openssl rand -hex 32) \
  -v barme:/data elroykanye/barme:0.7.0
```

## Known limits

- Alpha. The freeze is the intended contract and is largely in force, but formats
  may still shift before 1.0 — don't store anything you can't lose yet.
- The master key protects secrets at rest; object contents aren't encrypted.
- Doors bind `0.0.0.0`; reach a local instance via `127.0.0.1` (IPv4), not
  `localhost`. Put barme behind a firewall or reverse proxy.
- Single node. Image codecs (JPEG XL, AVIF) are routed but not yet transcoding.
