# barme 0.5.0

Security. barme no longer ships with a working default login, and access-key
secrets are encrypted on disk instead of sitting in plaintext. This closes the
"don't expose it yet" caveat that stood since the first release.

## What changed

- **No more default `barme/barme`.** The built-in credential is gone. On a fresh
  store with no credential configured, the daemon mints a random owner and prints
  it once at startup — save it, it isn't shown again. Set your own with
  `BARME_ACCESS_KEY` / `BARME_SECRET_KEY` (or `[credentials]` in `barme.toml`) and
  that's used instead. An exposed barme is no longer a known-password login.
- **Secrets encrypted at rest.** Access-key secrets are stored AES-256-GCM
  encrypted, not as plaintext JSON. Hashing isn't an option here — the S3 door
  verifies AWS SigV4, a symmetric HMAC, so the server must recover the raw secret
  to check a signature (the same reason AWS and MinIO keep recoverable secrets).
  Encryption keeps the key store free of plaintext while preserving S3
  compatibility: the secret is decrypted into memory only to verify a request.
- **Master key.** The encryption key is resolved from `BARME_MASTER_KEY` (64 hex
  chars), else a `master.key` file in the data dir (created `0600` on first boot),
  else freshly generated and announced. For real deployments set
  `BARME_MASTER_KEY` in the environment — keeping the key out of the data dir
  protects a stolen backup too. **Back the key up: encrypted secrets can't be
  recovered without it.**
- **Legacy plaintext key stores migrate automatically.** An existing store opened
  with a master key re-encrypts any plaintext secret records in place on startup.
- **Constant-time secret comparison** on the native door's Basic auth, closing a
  timing side channel where a `==` leaked how many leading bytes of a guess were
  right.

## Verified

- SigV4 still round-trips end to end against a secret that never touches disk in
  plaintext — encryption didn't break S3 compatibility.
- Durability and concurrency unaffected: the crash, abuse, and GC-under-load
  harnesses all pass on the new binary (every acknowledged object survives every
  crash; ~180 objects intact under `grace=0` GC).
- New tests cover encrypt/decrypt, wrong-key and tampered-ciphertext rejection,
  no-plaintext-on-disk, and legacy migration.

## Upgrading

Mostly drop-in, with one thing to know:

- **A `master.key` is created in your data dir on first boot** (or set
  `BARME_MASTER_KEY` yourself beforehand). Back it up. If you already had access
  keys, their secrets are migrated to encrypted form on this first start.
- If you relied on the `barme/barme` default, set `BARME_ACCESS_KEY` /
  `BARME_SECRET_KEY` explicitly, or grab the auto-generated credential from the
  startup output.

## Docker

```
docker run -p 7373:7373 -p 7374:7374 -p 7375:7375 -p 9000:9000 \
  -e BARME_MASTER_KEY=$(openssl rand -hex 32) \
  -e BARME_ACCESS_KEY=you -e BARME_SECRET_KEY=change-me \
  -v barme:/data elroykanye/barme:0.5.0
```

(Or run with no credential set and read the generated one from the logs.)

## Known limits

- Alpha. Formats and on-disk layout may still change before v1.
- The master key protects secrets at rest; it does not encrypt object contents.
- A pot name plus key must encode to under 255 bytes (about 120 key bytes for a
  short pot).
- `barmed` binds IPv4 (`0.0.0.0`); on a dual-stack host reach it via `127.0.0.1`
  rather than `localhost`.
- Single node. Image codecs (JPEG XL, AVIF) are routed but not yet transcoding.
