# barme 0.8.0

Bucket-level S3 operations and a Helm chart — barme is now something you can
point an S3 client's full preflight at, and deploy to Kubernetes in one command.

## What changed

- **S3 bucket operations.** The S3 door gained the verbs it was missing:
  - `PUT /{pot}` — CreateBucket (idempotent; a repeat is 200)
  - `HEAD /{pot}` — HeadBucket (200 if it exists, 404 otherwise)
  - `DELETE /{pot}` — DeleteBucket (409 if the pot still holds objects, else 204)
  - `GET /` — ListBuckets (XML list of every pot)

  Pots stay implicit for writes — a first PUT to an unknown pot still lands with
  the default policy — but these give a pot an explicit identity when a client
  provisions or lists one. A pot exists if it has a config or holds objects.
  DeleteBucket refuses a non-empty pot and does the empty-check and delete
  atomically (under the commit locks), so a write racing the delete can't have
  its just-acknowledged object silently wiped.
- **Helm chart (`charts/barme`).** `helm install barme ./charts/barme` runs a
  single-node store on Kubernetes: Deployment (one replica, `Recreate`, since the
  data volume is ReadWriteOnce), a PVC for `/data`, a Secret holding the owner
  credential and master key, a Service for all four doors, and an optional
  (SigV4-safe, no path-rewrite) ingress. Blank credentials are generated on first
  install and preserved across upgrades. **Argo CD / `helm template` users:** pin
  `auth.masterKey` (or use `auth.existingSecret`) — see the chart README, a
  regenerated master key makes stored secrets unrecoverable.

## Compatibility

Drop-in over 0.7.0. New endpoints only; the 0.7.0 on-disk format and stable API
are unchanged. `docs/STABILITY.md` lists the bucket verbs under the (S3-tracks-AWS)
stable surface.

## Docker

```
docker run -p 7373:7373 -p 7374:7374 -p 7375:7375 -p 9000:9000 \
  -e BARME_MASTER_KEY=$(openssl rand -hex 32) \
  -v barme:/data elroykanye/barme:0.8.0
```

## Known limits

- Alpha. Formats may still shift before 1.0 — don't store anything you can't lose.
- `GET /{pot}` (S3 ListObjects) is not implemented yet; use the native
  `/pots/{pot}/objects` to list.
- The master key protects secrets at rest; object contents aren't encrypted.
- Doors bind `0.0.0.0`; reach a local instance via `127.0.0.1` (IPv4).
- Single node. Image codecs (JPEG XL, AVIF) are routed but not yet transcoding.
