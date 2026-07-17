# barme 0.4.0

Durability. A write that barme acknowledged now survives a hard kill of the
process, byte-for-byte, and the server restarts clean on the same data dir. This
is the first of the changes that make a v1 mean something.

## What changed

- **Durable atomic writes.** Every store write already synced the file's contents
  and swapped it in with an atomic rename. It now also **fsyncs the containing
  directory after the rename**, so the rename itself survives power loss — a
  synced file whose directory entry was lost is still a lost file. New shard
  directories sync their parent too. (No-op on Windows, where a directory isn't
  fsync-able and NTFS journals its own metadata; Linux is the deploy target.)
- **Crash recovery on startup.** A process killed between creating a temp file and
  renaming it used to strand that temp file in a shard directory — and because
  chunk names are hashes, the stray file could trip the garbage collector's walk.
  Writes now use a known temp prefix, the shard walkers skip anything that isn't a
  chunk, and **startup reaps any leftover temp files** and logs how many. A clean
  shutdown recovers zero.
- **Proven, not asserted.** A new `kill -9` harness (`scripts/crash-test.sh`) runs
  rounds of concurrent uploads, hard-kills the daemon mid-write, restarts, and
  checks that every acknowledged object still downloads and matches its hash.
  Verified: **8 crash cycles, 47 acknowledged objects, zero lost or corrupted**,
  with recovery reaping stranded temp files every round.

## Upgrading

Drop-in. On-disk format is unchanged; an existing 0.3.0 data dir opens as-is. The
first start after an unclean 0.3.0 shutdown will log a one-line recovery notice if
it reaps any temp files.

## Running it

Download the binary for your platform below, then `./barmed`. Console on
`http://localhost:7374`, API on `:7373` (docs at `:7373/docs`), S3 on `:9000`,
CDN on `:7375`. Default login `barme:barme`; override with `BARME_ACCESS_KEY` /
`BARME_SECRET_KEY`.

## Docker

```
docker run -p 7373:7373 -p 7374:7374 -p 7375:7375 -p 9000:9000 -v barme:/data elroykanye/barme:0.4.0
```

## Known limits

- Alpha. Formats and on-disk layout may still change before v1.
- Secret keys are still stored in the clear and the default `barme/barme` login
  still works out of the box — both land in the next release. Don't expose it yet.
- A pot name plus key must encode to under 255 bytes (about 120 key bytes for a
  short pot).
- `barmed` binds IPv4 (`0.0.0.0`); on a dual-stack host reach it via `127.0.0.1`
  rather than `localhost`.
- Image codecs (JPEG XL, AVIF) are routed but not yet transcoding. Single node.
