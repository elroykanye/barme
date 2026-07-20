# barme 0.9.0

The operability pass: barme is now something you can watch, probe, and back up
with confidence — the last of the three v1 promises ("run it, watch it, back it
up, recover it"). The remaining gaps before 1.0 are closed.

## What changed

- **Backup and restore, documented and proven.** The data directory is a
  self-contained, coherent backup target; `docs/USAGE.md` covers how to snapshot
  it, where the master key must live, and how to restore. A test copies a
  populated data dir elsewhere, reopens it, and confirms every object reads back
  byte-for-byte with history and integrity intact.
- **Readiness endpoint.** `GET /ready` returns 503 when the store can't be read,
  distinct from `GET /health` (liveness). The Helm chart's readiness probe now
  points at it.
- **Richer metrics.** `/metrics` adds GC counters — sweeps, chunks condemned,
  chunks erased, and reachable chunks from the last sweep — plus logical bytes
  alongside physical, so dedup savings and GC activity are visible to Prometheus.
- **Server-side verify in the crash harness.** After every crash, the harness now
  cross-checks each acknowledged object against the server's own `POST /verify`,
  so recovery (download + re-hash) and the store's integrity check must agree.
- **Fuzzed name handling.** A property test throws arbitrary unicode, slashes, and
  lengths straddling the 255-byte filename bound at `put`/`get`: they either
  round-trip or reject cleanly, never panic.
- **Structured request logs.** Requests carry `method`/`uri`/`status`/`latency`
  through the HTTP trace layer, kept at debug so health probes don't spam the log;
  enable with `RUST_LOG=…,tower_http::trace=debug`.

## Compatibility

Drop-in over 0.8.0. New endpoint (`/ready`) and metrics only; on-disk format and
the stable API are unchanged. Helm chart bumped to 0.2.0 / appVersion 0.9.0.

## Docker

```
docker run -p 7373:7373 -p 7374:7374 -p 7375:7375 -p 9000:9000 \
  -e BARME_MASTER_KEY=$(openssl rand -hex 32) \
  -v barme:/data elroykanye/barme:0.9.0
```

## Known limits

- Alpha, but close: with this pass the v1 blocks (durability, security, format +
  API freeze, ops) are all done. A 1.0 is a decision away.
- `GET /{pot}` (S3 ListObjects) is still not implemented; use native
  `/pots/{pot}/objects`.
- Object contents aren't encrypted (secrets at rest are).
- Single node. Image codecs (JPEG XL, AVIF) are routed but not yet transcoding.
