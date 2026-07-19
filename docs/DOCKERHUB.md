# barme

A content-addressed object store that speaks S3.

barme splits every upload into content-defined chunks and addresses each chunk
and each object by its blake3 hash. An object is a Merkle tree of those hashes.
Storing things this way gives you dedup, cheap versioning, integrity checks on
read, and efficient sync — without extra machinery.

## Run it

    docker run -p 7373:7373 -p 7374:7374 -p 7375:7375 -p 9000:9000 \
      -v barme:/data elroykanye/barme:latest

- Web console: http://localhost:7374
- Native JSON API: http://localhost:7373 (docs at `/docs`)
- CDN: http://localhost:7375
- S3 API: http://localhost:9000

No default login: on first start with none set, barme mints a random owner and
prints it once in the logs. Set your own with `BARME_ACCESS_KEY` and
`BARME_SECRET_KEY`. Access-key secrets are encrypted at rest with a master key
(`BARME_MASTER_KEY`, else a `0600` `master.key` in `/data`) — back that key up.

Store and read an object:

    curl -u barme:barme -T photo.jpg http://localhost:7373/objects/photos/cat.jpg
    curl -u barme:barme http://localhost:7373/objects/photos/cat.jpg -o out.jpg

## Tags

- `latest` — most recent release
- `0.5.0`, `0.5.x` — pinned versions

## Image

A static musl binary on Alpine with CA roots only. Data lives in `/data`
(declared as a volume). Ports 7373, 7374, 7375, and 9000 are exposed.

## Config

Set with environment variables or a mounted `barme.toml`:

- `BARME_ACCESS_KEY`, `BARME_SECRET_KEY` — credentials
- `BARME_DATA_DIR` — data location (default `/data`)
- `BARME_EMBED_URL`, `BARME_EMBED_MODEL` — optional semantic search, proxied to
  your own embedder

## Status

Alpha. Works end to end, but it's early: uploads and downloads stream (memory
stays flat regardless of object size), an acknowledged write survives a hard kill
(writes are fsync-durable, the daemon recovers on restart), there's no default
login (a random owner is minted on first boot), and access-key secrets are
encrypted at rest. Formats and on-disk layout may still change before v1, and
object contents aren't encrypted. Don't put anything you can't lose in it yet.

## Links

- Source and full docs: https://github.com/elroykanye/barme
- Usage guide: https://github.com/elroykanye/barme/blob/main/docs/USAGE.md
- Architecture: https://github.com/elroykanye/barme/blob/main/docs/ARCHITECTURE.md

MIT licensed.
