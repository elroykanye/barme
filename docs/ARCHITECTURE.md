# Barme Architecture

Design notes for the storage engine, the compression model, garbage collection, the two APIs, and the semantic layer. This describes the design as built: barme shipped **v1.0** with the on-disk format and API frozen (see [STABILITY.md](STABILITY.md)). A few pieces below are still design-only or experimental — semantic search, image/perceptual codecs, and cross-node distribution — and are flagged where they appear.

## Contents

- [Model](#model)
- [The four building blocks](#the-four-building-blocks)
- [Object manifests](#object-manifests)
- [Write path](#write-path)
- [Compression](#compression)
- [Garbage collection](#garbage-collection)
- [The two APIs](#the-two-apis)
- [Semantic layer](#semantic-layer)
- [Multi-tenancy](#multi-tenancy)
- [Distribution (v2)](#distribution-v2)
- [Open questions](#open-questions)

## Model

One engine, two front doors.

```
   S3 clients                     native clients
        |                                |
   [ S3 API ]                     [ native API ]
        |                                |
        +--------------+-----------------+
                       |
                  [ engine ]   chunks, manifests, merkle, GC
                       |
                  [ storage ]
```

The engine is the whole system: content-defined chunking, hashing, merkle manifests, dedup, and garbage collection. The two APIs are translators in front of it. Neither one holds storage logic of its own, which is what keeps them consistent: an object written through one door behaves identically through the other.

## The four building blocks

Everything in the store is one of these, and each is addressed by its own hash.

| Block | What it is | Addressed by |
|-------|------------|--------------|
| Chunk | A slice of a file, cut by FastCDC | `hash(chunk bytes)` |
| Chunk list | Ordered list of chunk hashes that reassemble a file | `hash(list)` |
| Manifest | Record of how an object was stored; points at a chunk list | `hash(manifest)` — this is the `object_id` |
| Version pointer | A `bucket/key` label pointing at one manifest | mutable |

Everything is immutable and content-addressed except the version pointer. That single mutable label is the only thing that changes when an object is updated, which is what makes writes cheap and rollbacks trivial.

## Object manifests

Every object carries a manifest describing exactly how it was stored. This is the keystone of the design: reads are driven by the manifest, never by the current server config.

```json
{
  "manifest_version": 1,
  "object_id": "blake3:9f2a...c71",
  "created_at": "2026-07-16T10:22:04Z",
  "original": {
    "size_bytes": 26214400,
    "sha256": "e3b0c4...",
    "content_type": "image/jpeg"
  },
  "storage": {
    "route": "image",
    "fidelity": "exact",
    "codec": "jxl",
    "codec_params": { "effort": 9, "lossless_jpeg_transcode": true },
    "stored_size_bytes": 18874368,
    "reconstructs_original": true
  },
  "chunking": { "algo": "fastcdc", "chunks": ["blake3:aa1", "blake3:bb2"] },
  "quality": { "metric": null, "score": null },
  "tenant": "acme-corp",
  "policy_snapshot": "photos-bucket@v3"
}
```

Notable fields:

- `object_id` — the content address, the merkle root. How the engine finds and verifies the object.
- `original.sha256` — fingerprint of the true original bytes. On an exact read, the reconstructed output is re-hashed and checked against this, which proves the returned bytes match what came in.
- `storage.fidelity` — `exact` or `perceptual`. The single most important field. `exact` means the download equals the original. `perceptual` means it looks identical but is a different file.
- `storage.reconstructs_original` — the honest boolean. `true` for exact tiers, `false` for lossy ones. Drives the "restorable exactly" vs "visually identical" distinction in the UI.
- `policy_snapshot` — which bucket policy was active at write time. This is what lets config drift over time without breaking stored data.

The operating rule: **config is consulted only on write, to decide a new object's tier. Reads follow the object's own manifest.** Change defaults next year or add a codec in a later version, and everything already stored still restores, because each object carries its own instructions.

### On-disk format and compatibility

The data directory carries a `format.json` stamping the layout version, and every manifest carries its `manifest_version`. On open, a directory or object written by a *newer* barme is refused rather than misread; an older format runs a migration and is rewritten. That version stamp plus the manifest's self-description is what lets 1.x evolve the layout without orphaning data written by an earlier 1.x — the compatibility contract is spelled out in [STABILITY.md](STABILITY.md). Access-key secrets are the one recoverable thing that isn't public: they're encrypted at rest (AES-256-GCM) under a master key, since the S3 door verifies SigV4 and needs the raw secret to check a signature. Object *contents* are not encrypted.

## Write path

### Uploading `holiday.mp4` (v1)

A 25 MB video, bottom to top:

```
1. Chunk it       holiday.mp4 -> [C1][C2][C3][C4][C5]   (FastCDC, ~5 MB each)
2. Hash chunks    C1->aa1  C2->bb2  C3->cc3  C4->dd4  C5->ee5
3. Store new      all 5 are new -> all written
4. Chunk list     [aa1,bb2,cc3,dd4,ee5] -> L100
5. Manifest       {chunklist:L100, codec, fidelity, sha256} -> M900   (object_id)
6. Move pointer   mybucket/holiday.mp4 -> M900
```

### Uploading v2 (first few seconds edited)

The start of the file changes; everything after is untouched.

```
1. Chunk it       holiday.mp4 v2 -> [C1'][C2][C3][C4][C5]
2. Hash chunks    C1'->ff6 (new)   C2..C5 -> bb2,cc3,dd4,ee5 (already stored)
3. Store new      only C1' written  (~5 MB, not 25)
4. Chunk list     [ff6,bb2,cc3,dd4,ee5] -> L200
5. Manifest       -> M950
6. Move pointer   mybucket/holiday.mp4 -> M950   (M900 still exists)
```

A 25 MB "new version" costs about 5 MB on disk. The old version isn't destroyed; the pointer just moved. Listing versions means listing every manifest the pointer has aimed at, and rolling back means pointing it at an old manifest again.

### Why content-defined boundaries matter

If chunks were cut at fixed 5 MB offsets, editing the start would shift every later byte, changing every chunk's contents and every hash, and dedup would save nothing. FastCDC cuts at boundaries chosen by the content itself, so an edit only disturbs the chunks it actually touches and the downstream cut points re-sync. That "a local edit stays local" property is the reason versioning and dedup work at all.

## Compression

Set per bucket, recorded per object. Two routes, picked by content type, because whole-file image codecs and content-defined chunking pull in opposite directions.

- **Blob route** (backups, documents, archives, unknown types): chunk with FastCDC on the original bytes, compress each chunk on its own, dedup at the chunk level. A stored chunk is addressed by the hash of its compressed bytes, which keeps the chunk store a pure self-verifying blob store. Whole-object integrity is a separate SHA-256 of the original recorded in the manifest and checked on read.
- **Image route** (photos, media): treat the file as a whole, apply an image codec, dedup at the file level.

Fidelity tiers:

| Tier | Codec | Fidelity | Notes |
|------|-------|----------|-------|
| 1 | zstd | exact | Default floor. Nothing is stored uncompressed. |
| 2 | JPEG XL lossless transcode | exact | JPEGs ~20-30% smaller, original rebuildable byte for byte. |
| 3 | JPEG XL / AVIF lossy | perceptual | Visually identical, smaller, not byte-identical. |
| 4 | neural codecs | perceptual | Not in scope yet. The manifest design leaves room to add it later. |

For tier 3, browser reach differs: AVIF is broadly supported, JPEG XL is not, so served-to-browser buckets should lean AVIF and cold-archive buckets can prefer JPEG XL. Perceptual writes record a quality metric (SSIM / VMAF / Butteraugli) and score in the manifest, so "how faithful was this" is a stored fact, not a guess.

## Garbage collection

Chunks are shared, so deleting an object cannot mean erasing its chunks. Some of them may belong to other objects or other versions. The real question is whether a chunk is still reachable by anything at all.

Barme uses **mark-and-sweep with a grace period**, not reference counting. Reference counting is faster but keeps a second copy of the truth that drifts under crashes and concurrency, and drift there means silent data loss. Mark-and-sweep re-derives reachability from the live pointers each pass, so it's self-correcting. Its cost is CPU, which is an optimization problem rather than a correctness one.

```
MARK   From a snapshot of every live version pointer, walk
       pointer -> manifest -> chunk list -> chunks, marking each reachable.

SWEEP  Any chunk not marked is unreachable. Condemn it (stamp condemned_at),
       don't erase it yet.

ERASE  A later pass deletes chunks that have been condemned longer than the
       grace window and are still unreachable.
```

The dangerous case is a chunk that's unreferenced during MARK but gets reused by a concurrent upload via dedup before SWEEP. Deleting it would corrupt the new object. Defenses:

- **Grace period.** Condemned chunks aren't erased for a window (e.g. 24h). Any write that references a condemned chunk clears the stamp (resurrection).
- **Write-then-reference ordering.** An upload fully writes its chunks before moving the pointer, and GC never condemns a chunk younger than the grace window, so a just-written chunk is never eligible even before a manifest points at it.
- **Snapshot roots.** MARK runs against a frozen set of live pointers, so a pointer moving mid-sweep can't hide a manifest.

## The two APIs

### S3 API

The bucket/key/object model maps almost directly onto bucket/pointer/manifest.

| S3 request | Engine action |
|------------|---------------|
| `PUT bucket/key` | chunk, dedup, write new chunks, manifest, move pointer |
| `GET bucket/key` | pointer -> manifest -> reassemble -> decompress per manifest -> verify -> serve |
| `DELETE bucket/key` | move/clear the pointer; chunks reclaimed later by GC, never inline |
| `HEAD bucket/key` | read manifest; etag is the content hash; `X-Barme-Object-Id` carries the content id |
| Multipart | each part is a batch of chunks; completion re-hashes and assembles one manifest |
| `PUT/HEAD/DELETE bucket`, `GET /` | create / head / delete a pot; ListBuckets |

Implemented as of v1.0: object `PUT/GET/DELETE/HEAD`, the full multipart sequence, and bucket create/head/delete plus ListBuckets, all with AWS SigV4. Not yet on the S3 door, tracked for 1.1: `ListObjects` (`GET bucket?list-type=2`), `?versionId` reads, and presigned query-string URLs — barme's own share links already cover time-limited delivery through the CDN door. The long tail of bucket sub-resources (ACLs, lifecycle, policies) comes later.

### Native API

Exposes what S3 has no vocabulary for. The live, authoritative reference is served at `/docs`; the highlights:

- `GET /history/{pot}/{key}` — the full version graph, diffable; `/diff` returns only the chunks that differ, `/restore` rolls the pointer back.
- `GET /manifest/{pot}/{key}` — fidelity, codec, quality: is this object exact or perceptual, and how faithful. `POST /verify` re-hashes it against its manifest.
- `GET /content/{hash}` — fetch any object or chunk directly by hash.
- Sync: `POST /sync/plan` (manifest + missing chunks), `GET/PUT /chunk/{hash}`, `POST /sync/import/{pot}/{key}`, plus `/proof` and `/delta` — the content-addressed replication primitives (see [Distribution](#distribution-v2)). *(experimental)*
- `POST /search`, `POST /similar/{hash}` — semantic retrieval (see below). *(experimental)*
- Ops: `/health` (liveness), `/ready` (readiness), `/metrics` (Prometheus, owner-authed).

## Semantic layer

A vector index keyed by content hash, storing what an object means alongside where its bytes live. The content hash is the join key between the two.

- **Off the write path.** Understanding is enqueued after the pointer moves and runs asynchronously. Uploads never wait on it.
- **Routed by content type.** Images get a vision embedding plus optional caption/OCR; audio and video get transcripts plus embeddings; documents get text embeddings; opaque blobs are skipped.
- **Deduplicated by hashing.** The same content is embedded once, no matter how many times it's uploaded. The trick that dedups bytes dedups the GPU work too.
- **Derived and disposable.** Every embedding can be rebuilt from the stored bytes. Losing the index is a rebuild, not data loss. It's versioned by model, so a better model can re-embed in the background while the old vectors stay usable.

Search accepts text, an example image, or a hybrid, and resolves to ranked `object_id`s that the engine then serves.

Deployment: the index runs as a separate optional service the engine talks to, since vector search has different memory and hardware needs than byte storage. It stays an index over the store, never a source of truth.

## Multi-tenancy

- **Keyed deduplication.** Content-defined chunking can leak whether a chunk already exists, which lets one tenant probe for another's data. Dedup is scoped per tenant with a keyed hash, so chunks only dedup within a tenant.
- **Scoped search.** The semantic index carries the tenant and filters on it before nearest-neighbor, so search never crosses tenant boundaries.

## Distribution (v2)

barme 1.x is **single node**: one engine, one data directory, one volume.
Durability comes from fsync-durable writes plus backups and the underlying
volume, not from replication — there is no erasure coding, multi-node quorum, or
automatic failover, and the deployment is pinned to a single replica on purpose.
This is deliberate; a trustworthy single node is what v1 set out to be.

Distribution is the **v2 headline**, deferred on purpose because content
addressing makes it cheap to add *later*: chunks are immutable and hash-named, so
replication is "copy the chunks the peer doesn't have" — already the sync
protocol (`/sync/plan`, `/chunk`, `/sync/import`, `/proof`, `/delta`). No mutation
conflicts, no rebalancing. The staged path and rationale live in
[V1.md](V1.md#explicitly-v2-not-v1-distribution); the design is intentionally not
worked out here yet.

## Open questions

Resolved since v1.0: the on-disk layout (sharded content-addressed directories under the data dir, versioned by `format.json`), GC scheduling (a configurable grace window and sweep interval), and the license (MIT). Still open:

- S3 surface: the long tail (ACLs, lifecycle rules, bucket policies), plus `ListObjects` and presigned query-string auth (tracked for 1.1).
- Pluggable storage backends for chunks (today: local disk only).
- Embedding models per content type, and where inference runs (the semantic layer is experimental).
- Cross-node durability — erasure coding vs. replication — folded into the [Distribution (v2)](#distribution-v2) work.
