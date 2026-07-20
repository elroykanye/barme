# Getting started

## Run it

Docker:

    docker run -p 7373:7373 -p 7374:7374 -p 7375:7375 -p 9000:9000 -v barme:/data elroykanye/barme:1.0.0

Or download a `barmed` binary from the [releases](https://github.com/elroykanye/barme/releases) and run `./barmed`. From source: `cargo run -p barmed --features ui`.

Ports:

| Port | Door |
|------|------|
| 7373 | Native JSON API |
| 7374 | Web console |
| 7375 | CDN (public and by-hash delivery) |
| 9000 | S3 API |

The console is at http://localhost:7374. There is no default login: on first
start with no credential configured, barme mints a random owner (access key
`barme`, a random secret) and prints it once in the startup output — copy it
then. Set your own with `BARME_ACCESS_KEY` and `BARME_SECRET_KEY` (or
`[credentials]` in `barme.toml`) to skip the generated one. `barmed --help` lists
the flags.

## Store and read an object

The native API is JSON over HTTP with Basic auth. A **pot** is a container; it's created the first time you write to it.

Upload:

    curl -u barme:barme -T photo.jpg -H "Content-Type: image/jpeg" \
      http://localhost:7373/objects/photos/cat.jpg

Download:

    curl -u barme:barme http://localhost:7373/objects/photos/cat.jpg -o cat.jpg

List pots, then objects in a pot:

    curl -u barme:barme http://localhost:7373/pots
    curl -u barme:barme http://localhost:7373/pots/photos/objects

Delete:

    curl -u barme:barme -X DELETE http://localhost:7373/objects/photos/cat.jpg

The full endpoint list is served at http://localhost:7373/docs.

## Versions and integrity

Every write keeps the previous version. Nothing is overwritten in place.

    curl -u barme:barme http://localhost:7373/history/photos/cat.jpg    # version ids
    curl -u barme:barme http://localhost:7373/manifest/photos/cat.jpg   # how it was stored
    curl -u barme:barme -X POST http://localhost:7373/verify/photos/cat.jpg   # re-hash check

Roll back to an earlier version by its id:

    curl -u barme:barme -X POST http://localhost:7373/restore/photos/cat.jpg \
      -H 'content-type: application/json' -d '{"object_id":"blake3:..."}'

## S3 API

The S3 door handles object PUT, GET, DELETE, and HEAD with AWS SigV4, the
multipart upload sequence, and bucket create/head/delete plus ListBuckets. Point
any S3 client at http://localhost:9000 with path-style addressing.

    aws configure set aws_access_key_id barme
    aws configure set aws_secret_access_key barme
    aws --endpoint-url http://localhost:9000 s3 mb s3://photos
    aws --endpoint-url http://localhost:9000 s3 cp photo.jpg s3://photos/cat.jpg
    aws --endpoint-url http://localhost:9000 s3 cp s3://photos/cat.jpg out.jpg

Object listing (`GET /{pot}`, S3 ListObjects) isn't on the S3 door yet; use the native `/pots/{pot}/objects` endpoint to list a pot's contents.

## Config

barme runs on defaults with no config. To change them, put a `barme.toml` next to the binary, or point at one with `--config` or `BARME_CONFIG`:

    data_dir     = "./barme-data"
    native_addr  = "0.0.0.0:7373"
    s3_addr      = "0.0.0.0:9000"
    cdn_addr     = "0.0.0.0:7375"
    console_addr = "0.0.0.0:7374"
    # Largest accepted single object, in bytes. Uploads stream (memory stays
    # flat), so this is a size ceiling, not a memory one. Over it gets 413.
    # Default 512 MiB.
    max_upload_bytes = 536870912
    # Browser origins allowed to call the API and CDN doors. Default ["*"] allows
    # any (convenient locally). In production list your console/app origins so
    # other sites can't script the API from a victim's browser.
    cors_origins = ["https://console.example.com"]

    [credentials]
    access_key = "barme"
    secret_key = "change-me"

    [default_policy]
    codec      = "zstd"   # or "none"
    zstd_level = 0         # 0 = zstd's default level

    # Optional semantic search: point at your own embedder. No models run here.
    # embed_url   = "http://localhost:11434/api/embeddings"
    # embed_model = "nomic-embed-text"

Environment variables override the file: `BARME_DATA_DIR`, `BARME_ACCESS_KEY`, `BARME_SECRET_KEY`, `BARME_MASTER_KEY`, `BARME_EMBED_URL`, `BARME_EMBED_MODEL`, `BARME_MAX_UPLOAD_BYTES`. If a port is already taken, barme rolls forward to the next free one.

## Credentials and the master key

There is no default login. On first start with none configured, barme mints a random owner and prints it once — save it. Set `BARME_ACCESS_KEY` / `BARME_SECRET_KEY` (or `[credentials]` in `barme.toml`) to use your own.

Access-key secrets are encrypted at rest (AES-256-GCM), not stored in plaintext. Secrets can't be hashed here: the S3 door verifies AWS SigV4, which needs the raw secret to recompute the signature, so the server keeps a recoverable — but encrypted — copy. The encryption key (the "master key") is resolved in this order:

1. `BARME_MASTER_KEY` — 64 hex characters (32 bytes). Best for real deployments: keeping the key in the environment, out of the data dir, protects a stolen backup too. Generate one with `openssl rand -hex 32`.
2. `<data_dir>/master.key` — created `0600` on first boot if you didn't set the env var.
3. Otherwise generated fresh on first boot and announced in the log.

**Back the master key up.** Encrypted secrets can't be recovered without it. An existing plaintext key store is migrated to encrypted form automatically the first time it's opened with a master key.

Keys are encoded into a filename, so a pot name plus key must stay under the filesystem's 255-byte filename limit (about 120 key bytes for a short pot). Longer keys are rejected with `400`.

Per-pot settings (compression, public read, lifecycle) are set over the API (use your credential in place of `KEY:SECRET`):

    curl -u KEY:SECRET -X PUT http://localhost:7373/pots/photos/config \
      -H 'content-type: application/json' -d '{"public_read": true, "codec": "zstd"}'

## Sync between two instances

barme replicates an object by transferring only the chunks the far side doesn't already have. Given a source `S` and destination `D` (both owner-authed), to copy object `blake3:ID`:

    # 1. ask S for the manifest and which chunks D is missing
    curl -u barme:barme -X POST http://S:7373/sync/plan \
      -H 'content-type: application/json' \
      -d '{"object_id": "blake3:ID", "have": []}'
    #    -> { "manifest": {...}, "missing": ["blake3:...", ...] }

    # 2. copy each missing chunk S -> D (re-verified by hash on receipt)
    curl -u barme:barme http://S:7373/chunk/blake3:H | \
      curl -u barme:barme -T - http://D:7373/chunk/blake3:H

    # 3. adopt the manifest on D; it now serves the object
    curl -u barme:barme -X POST http://D:7373/sync/import/photos/cat.jpg \
      -H 'content-type: application/json' -d @manifest.json

Prove a specific chunk belongs to an object, without shipping the whole object:

    curl -u barme:barme "http://localhost:7373/proof/photos/cat.jpg?index=0"

## Delivery links

The CDN door (7375) serves object bytes over three URL shapes:

- `GET /cdn/{hash}` — immutable, by content hash. Served `Cache-Control:
  immutable, max-age=1y`, so it caches forever at every layer; the hash is the
  capability (anyone holding it can fetch).
- `GET /public/{pot}/{key}` — only for pots marked public; revalidated with an ETag.
- `GET /s/{pot}/{key}?exp&sig` — a time-limited signed share of a private object;
  short-lived and revalidated.

Get the hash for a `/cdn` link from the **`X-Barme-Object-Id`** response header.
Every write returns it — a single PUT and a multipart complete alike — and HEAD
returns it for a known key. Use this rather than the ETag: an S3 multipart ETag
is a digest of the part digests, not the object's own hash.

> **`/cdn/{hash}` is permanent and cannot be revoked.** Because it caches
> forever, deleting an object at the origin does not stop caches that already hold
> the bytes from serving them for up to a year. **Never serve erasable or personal
> data over `/cdn/{hash}`** — if you might have to honor a deletion, serve it over
> `/s/{pot}/{key}` (short-lived, revalidated) instead. `/cdn` is for public,
> non-erasable content.

## Observability

Probe and telemetry endpoints on the native door (7373):

- `GET /health` — liveness plus headline counts (objects, pots, chunks, uptime).
  No auth, safe for a probe.
- `GET /ready` — readiness: 200 when the store is readable, 503 if the data dir
  can't be read. No auth. Point a Kubernetes readiness probe here, liveness at
  `/health`.
- `GET /metrics` — Prometheus text, **owner-authed**: object/pot/chunk counts,
  logical vs physical bytes, and GC counters (sweeps, condemned, erased, live).

Logs go to stderr, filtered by `RUST_LOG` (default `info`). Request logging is
structured — each request carries `method`, `uri`, `status`, and `latency` — and
lives at debug on the HTTP trace layer, off by default so health probes don't
spam the log. Turn it on with:

    RUST_LOG=info,tower_http::trace=debug barmed

## Backup and restore

The data directory is a self-contained, coherent backup target. Everything barme
needs lives under it — chunks, manifests, version pointers, per-pot config, keys,
the format stamp, and (unless you set `BARME_MASTER_KEY`) the `master.key`. Writes
are atomic and fsync-durable, so once a write is acknowledged it survives in that
directory.

**Back up** the whole directory as one unit. For a consistent point-in-time copy,
either stop `barmed` first or take a filesystem/volume snapshot (an atomic
snapshot, or a copy of a stopped instance, captures a coherent set; a plain `cp`
of a *live* directory can catch a write mid-flight):

    # stopped instance, or a snapshot mount:
    tar czf barme-backup-$(date +%F).tgz -C /path/to data

**Restore** by putting the directory back and pointing barme at it. On open it
re-adopts the format stamp and reaps any temp files a crash left behind:

    tar xzf barme-backup-2026-07-20.tgz -C /path/to
    BARME_DATA_DIR=/path/to/data barmed

**The master key is the one thing that can live outside the backup.** If you run
with `BARME_MASTER_KEY` in the environment (recommended for production), the data
dir holds only *encrypted* secrets — so **back the master key up separately**, or
a restored copy can't decrypt its own keys. If instead you rely on the in-dir
`master.key`, it travels with the backup (self-contained, but the backup then
contains the key that decrypts its secrets — protect it accordingly).

A copied data dir opening clean and returning every object byte-for-byte is
covered by a test (`crates/barme-engine/tests/backup.rs`).

## Next

- Design and on-disk layout: [ARCHITECTURE.md](ARCHITECTURE.md)
- Live API reference: http://localhost:7373/docs
