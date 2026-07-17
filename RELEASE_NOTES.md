# barme 0.2.0

A hardening release. Same features as 0.1.0, but it no longer falls over under
pressure. Found by actually stress-testing a memory-capped instance with large,
concurrent, and hostile uploads.

## Fixed

- **A large upload no longer OOM-kills the server.** Uploads are buffered in
  memory, and the body was unbounded — a big enough one took the whole process
  down. Uploads are now capped (`max_upload_bytes`, default 512 MiB) and
  anything over the cap is rejected with `413 Payload Too Large` instead of
  crashing. Set it in `barme.toml` or with `BARME_MAX_UPLOAD_BYTES`.
- **Write memory roughly halved.** Chunking held a second full copy of every
  object in memory during a write (~2× the object's size). It now works over
  borrowed slices, so a write costs about 1× the object's size. A 200 MB upload
  that used to peak near 450–650 MB now peaks near 200 MB.
- **Over-long keys return `400`, not `500`.** Keys are encoded into a filename,
  so a pot name plus key that exceeds the filesystem's 255-byte filename limit
  used to fail deep in the store with a `500`. It's now checked up front and
  rejected cleanly.

## Unchanged and still solid

Under the same stress run, dedup held through 30 concurrent uploads of an
identical blob, 80 versions of one key tracked correctly, 800 objects landed
under 25-way parallelism, and unicode/space/`%`/`#`/path-traversal keys all
stored as literal keys. Those already worked; the run confirmed it.

## Running it

Download the binary for your platform below, then:

```
./barmed
```

Console on `http://localhost:7374`, API on `:7373` (docs at `:7373/docs`), S3 on
`:9000`, CDN on `:7375`. Default login `barme:barme`, override with
`BARME_ACCESS_KEY` / `BARME_SECRET_KEY`. `barmed --help` lists the flags.

## Docker

```
docker run -p 7373:7373 -p 7374:7374 -p 7375:7375 -p 9000:9000 -v barme:/data elroykanye/barme:0.2.0
```

Runtime image is now debian-slim (a shell for `docker exec`, no busybox).

## Known limits

- Alpha. Formats and on-disk layout may still change.
- Uploads still buffer the whole body; the cap keeps that safe, but true
  streaming (constant memory regardless of size) is the next release.
- A pot name plus key must encode to under 255 bytes; for a short pot that's
  about 120 key bytes.
- Image codecs (JPEG XL, AVIF) are routed but not yet transcoding.
- Single node.
