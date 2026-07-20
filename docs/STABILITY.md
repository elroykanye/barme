# Stability and compatibility

What barme promises not to break, and how it evolves the parts it hasn't frozen
yet. The full endpoint reference is served live at `/docs`; this is the contract
around it.

## The promise (from v1 on)

- **On-disk data written by one 1.x release reads on every later 1.x.** The data
  directory carries an on-disk format version (`format.json`) and every object
  carries a `manifest_version`. A breaking layout change bumps the format version
  and ships a migration that runs on open; additive changes (a new optional
  manifest field, a new subdirectory) don't bump anything, because old data still
  reads.
- **A newer directory is refused, not misread.** If a data dir or an object was
  written by a barme newer than the running one, barme refuses that dir (won't
  start) or that object (per-object error) rather than guess at a layout it
  doesn't know. Downgrade safely by keeping the newer binary.
- **The stable API surface below doesn't change shape within a major version.**
  Paths, methods, status codes, and response bodies stay compatible. New optional
  fields and new endpoints may be added; existing ones aren't removed or
  repurposed.

Until 1.0 this is the intended contract and is largely in force, but the alpha
caveat still holds: formats *may* still shift before 1.0, and nothing here is a
promise to keep data you can't afford to lose.

## Stable surface

Frozen as the v1 contract:

- **Objects & versions** — `/objects`, `/manifest`, `/history`, `/meta`,
  `/restore`, `/diff`, `/verify`, `/content/{hash}`, `/presign`
- **Pots** — `/pots` and its sub-resources (`rename`, `visibility`, `config`,
  `objects`, `import`, `zip`), `/ops/copy`, `/ops/move`
- **Access keys** — `/keys`
- **S3 door** — object PUT/GET/DELETE/HEAD, the multipart sequence, and bucket
  create/head/delete plus ListBuckets, AWS SigV4. (The S3 wire contract is AWS's;
  barme tracks it.)
- **CDN door** — `/cdn/{hash}`, `/public/{pot}/{key}`, `/s/...` (presigned share).
  Note: `/cdn/{hash}` caches permanently and can't be revoked, so it's for public,
  non-erasable content only — serve erasable/personal data over `/s/` (see
  "Delivery links" in `docs/USAGE.md`).
- **Ops** — `/health`, `/stats`, `/metrics`
- **On-disk layout** — the store directory structure and the v1 manifest schema
  (pinned by a content-hash test so a silent change fails CI)

## Experimental surface

Present and usable, but **may change between releases** — not covered by the v1
promise:

- **Semantic search** — `/search`, `/similar/{hash}`. Proxies to an external
  embedder; result shape will evolve.
- **Merkle & sync** — `/proof`, `/delta`, `/object/{id}`, `/chunk/{hash}`,
  `/sync/plan`, `/sync/import`. The replication primitives are maturing.
- **Webhooks** — `/webhooks`. Event payload shape may change.
- **Image codecs** — the `Image` route and `perceptual` fidelity. Routed and
  recorded in the manifest, but not yet transcoding.

Experimental endpoints are marked as such in the live `/docs`.
