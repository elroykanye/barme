# barme 1.1.0

A deployment can now tell the console where the API and the CDN really are.
Additive over 1.0: new optional settings, no change to the on-disk format and no
change to any existing behaviour.

## In this release

- **`console_api_url` / `console_cdn_url` (#11).** The console worked out its API
  base from the address bar plus a fixed port. That's right when it's reached
  directly and wrong the moment anything is put in front of it: served from
  `https://store.example.com` it called `http://store.example.com:7373` — a port
  a reverse proxy doesn't publish, over plain HTTP from an HTTPS page, which
  browsers block as mixed content even when the port is open. Sign-in then never
  completed, and because the blocked request breaks the padlock it read as a TLS
  fault, sending you to inspect a certificate that was perfectly fine.

  Nothing outside the server could fix it: `VITE_BARME_API` is a Vite
  compile-time constant and the console is baked into the binary, so changing it
  meant rebuilding the image; `cors_origins` covers only the CORS half; and
  barmed knew its bind addresses but had no notion of a public URL. Now it does.
  Set `console_api_url` and `console_cdn_url` in `barme.toml` (or
  `BARME_CONSOLE_API_URL` / `BARME_CONSOLE_CDN_URL`) and they're injected into
  `index.html` as it is served — at serve time, not build time, which is the
  whole point: a deployment sets them without rebuilding an image. The console
  prefers them over what it would otherwise derive.

  The values land inside a script tag, so they're escaped rather than trusted,
  with a test that a value can't close the tag it sits in.

## Compatibility

Drop-in over 1.0.0. Leave the new settings unset and not a byte of the served
page changes — the console behaves exactly as before, which is what you want on
localhost. Same on-disk format, same stable API (see `docs/STABILITY.md`).

## Upgrading behind a proxy

If you publish the console through a reverse proxy, set the two URLs and route
those paths to the native (7373) and CDN (7375) listeners:

    console_api_url = "https://store.example.com/api"
    console_cdn_url = "https://store.example.com/cdn"

## Docker

```
docker run -p 7373:7373 -p 7374:7374 -p 7375:7375 -p 9000:9000 \
  -e BARME_MASTER_KEY=$(openssl rand -hex 32) \
  -v barme:/data elroykanye/barme:1.1.0
```

## The road here

- 0.4.x — durability (fsync, crash recovery) and concurrency/GC hardening
- 0.5.x — security (encrypted secrets, no default login, presign, CORS)
- 0.6.0 — S3 multipart upload
- 0.7.0 — on-disk format version + API freeze
- 0.8.0 — S3 bucket operations, Helm chart
- 0.9.0 — operability (backup/restore, readiness, metrics, name fuzzing)
- 1.0.0 — frozen format and API, trustworthy with data, operable
- 1.1.0 — the console reachable from behind a reverse proxy
