# barme 0.5.1

Finishes the security block: the CORS config knob now works, and expiring share
links are verified end to end.

## What changed

- **`cors_origins` is now enforced.** It was a dead config field — both the API
  and CDN doors hardcoded permissive CORS, so any website could script the API
  from a visitor's browser regardless of config. Now `["*"]` (the default) stays
  permissive for local use, but a specific list restricts
  `Access-Control-Allow-Origin` to exactly those origins. Set your console/app
  origins in production.
- **Presigned share links, verified.** The CDN share door (`/s/{pot}/{key}`)
  checks the HMAC signature and the expiry, serving a private object only for a
  valid, unexpired link. Now covered by tests: a valid link serves; an expired
  one, a tampered signature, and a signature minted for a different key are each
  refused with `403`.

With these, the v1 security block is complete: no default login, secrets
encrypted at rest, constant-time auth compare, verified share links, and a real
CORS boundary.

## Upgrading

Drop-in. If you relied on the API being reachable from any browser origin, that
still works by default (`cors_origins = ["*"]`); set an explicit list to lock it
down.

## Docker

```
docker run -p 7373:7373 -p 7374:7374 -p 7375:7375 -p 9000:9000 \
  -e BARME_MASTER_KEY=$(openssl rand -hex 32) \
  -v barme:/data elroykanye/barme:0.5.1
```

## Known limits

- Alpha. Formats and on-disk layout may still change before v1.
- The master key protects secrets at rest; it does not encrypt object contents.
- Doors bind `0.0.0.0` by default — put barme behind a firewall or reverse proxy;
  reach a local instance via `127.0.0.1` (IPv4), not `localhost`.
- A pot name plus key must encode to under 255 bytes (about 120 key bytes for a
  short pot).
- Single node. Image codecs (JPEG XL, AVIF) are routed but not yet transcoding.
