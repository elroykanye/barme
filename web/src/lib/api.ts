// Thin client for the barme native API. Auth is Basic (owner access/secret),
// held in localStorage since this console runs on the owner's own machine.

// Default to the host the console was loaded from, so opening it over a LAN/WSL
// IP still reaches the API and CDN on the same host (not the browser's own
// localhost). Override with VITE_BARME_API / VITE_BARME_CDN at build time.
const HOST = typeof window !== "undefined" ? window.location.hostname || "localhost" : "localhost";
const BASE = import.meta.env.VITE_BARME_API ?? `http://${HOST}:7373`;
const CDN = import.meta.env.VITE_BARME_CDN ?? `http://${HOST}:7375`;

export type Creds = { access: string; secret: string };

export interface Stats {
  buckets: number;
  objects: number;
  logical_bytes: number;
  physical_bytes: number;
  unique_chunks: number;
}

export interface BucketInfo {
  name: string;
  public_read: boolean;
  objects: number;
}

export interface ObjectInfo {
  key: string;
  size: number;
  versions: number;
}

export interface Manifest {
  object_id: string;
  original: { size_bytes: number; content_type: string; sha256: string };
  storage: {
    route: string;
    fidelity: string;
    codec: string;
    stored_size_bytes: number;
    reconstructs_original: boolean;
  };
  chunking: { chunks: string[] };
  quality: { metric: string | null; score: number | null };
  tenant: string;
  policy_snapshot: string;
}

export interface SearchHit {
  id: string;
  score: number;
  pot?: string;
  key?: string;
}

export interface ObjectMeta {
  tags: Record<string, string>;
  note: string;
  favorite: boolean;
  locked_until: string | null;
}

export interface DiffResult {
  added: string[];
  removed: string[];
  shared: string[];
}

export interface Webhook {
  id: string;
  url: string;
  events: string[];
}

export interface Health {
  objects: number;
  pots: number;
  unique_chunks: number;
  uptime_secs: number;
}

export interface PotConfig {
  public_read: boolean;
  codec: string | null;
  zstd_level: number | null;
  fidelity: string | null;
  route_by_content_type: boolean;
  max_versions: number | null;
  expire_after_days: number | null;
}

export interface KeyInfo {
  access_key: string;
  read_only: boolean;
  pots: string[];
  created_at: string;
}

export interface NewKey {
  access_key: string;
  secret_key: string;
  read_only: boolean;
  pots: string[];
}

export class ApiError extends Error {
  constructor(
    public status: number,
    message: string,
  ) {
    super(message);
  }
}

const KEY = "barme.creds";

export function loadCreds(): Creds | null {
  const raw = localStorage.getItem(KEY);
  return raw ? (JSON.parse(raw) as Creds) : null;
}

export function saveCreds(c: Creds | null) {
  if (c) localStorage.setItem(KEY, JSON.stringify(c));
  else localStorage.removeItem(KEY);
}

function authHeader(): Record<string, string> {
  const c = loadCreds();
  return c ? { Authorization: "Basic " + btoa(`${c.access}:${c.secret}`) } : {};
}

function encodeKey(key: string): string {
  return key.split("/").map(encodeURIComponent).join("/");
}

async function req(path: string, opts: RequestInit = {}): Promise<Response> {
  const res = await fetch(BASE + path, {
    ...opts,
    headers: { ...authHeader(), ...(opts.headers ?? {}) },
  });
  if (!res.ok) {
    throw new ApiError(res.status, await res.text().catch(() => res.statusText));
  }
  return res;
}

export const api = {
  base: BASE,

  async listBuckets(): Promise<BucketInfo[]> {
    return (await req("/pots")).json();
  },

  async listObjects(bucket: string): Promise<ObjectInfo[]> {
    return (await req(`/pots/${encodeURIComponent(bucket)}/objects`)).json();
  },

  async setVisibility(bucket: string, publicRead: boolean): Promise<void> {
    await req(`/pots/${encodeURIComponent(bucket)}/visibility`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ public_read: publicRead }),
    });
  },

  async upload(bucket: string, key: string, file: File): Promise<{ object_id: string }> {
    return (
      await req(`/objects/${encodeURIComponent(bucket)}/${encodeKey(key)}`, {
        method: "PUT",
        headers: { "Content-Type": file.type || "application/octet-stream" },
        body: file,
      })
    ).json();
  },

  async remove(bucket: string, key: string): Promise<void> {
    await req(`/objects/${encodeURIComponent(bucket)}/${encodeKey(key)}`, {
      method: "DELETE",
    });
  },

  async download(bucket: string, key: string): Promise<Blob> {
    return (await req(`/objects/${encodeURIComponent(bucket)}/${encodeKey(key)}`)).blob();
  },

  async manifest(bucket: string, key: string): Promise<Manifest | null> {
    const res = await req(`/manifest/${encodeURIComponent(bucket)}/${encodeKey(key)}`);
    return res.status === 404 ? null : res.json();
  },

  async history(bucket: string, key: string): Promise<string[]> {
    return (await req(`/history/${encodeURIComponent(bucket)}/${encodeKey(key)}`)).json();
  },

  async search(query: string): Promise<SearchHit[]> {
    const res = await req("/search", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ query }),
    });
    return res.status === 501 ? [] : res.json();
  },

  async stats(): Promise<Stats> {
    return (await req("/stats")).json();
  },

  async renameBucket(bucket: string, newName: string): Promise<void> {
    await req(`/pots/${encodeURIComponent(bucket)}/rename`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ new_name: newName }),
    });
  },

  async deleteBucket(bucket: string): Promise<void> {
    await req(`/pots/${encodeURIComponent(bucket)}`, { method: "DELETE" });
  },

  async moveObject(fromB: string, fromK: string, toB: string, toK: string): Promise<void> {
    await ops("/ops/move", fromB, fromK, toB, toK);
  },

  async copyObject(fromB: string, fromK: string, toB: string, toK: string): Promise<void> {
    await ops("/ops/copy", fromB, fromK, toB, toK);
  },

  async getPotConfig(pot: string): Promise<PotConfig> {
    return (await req(`/pots/${encodeURIComponent(pot)}/config`)).json();
  },

  async setPotConfig(pot: string, cfg: PotConfig): Promise<void> {
    await req(`/pots/${encodeURIComponent(pot)}/config`, {
      method: "PUT",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(cfg),
    });
  },

  async listKeys(): Promise<KeyInfo[]> {
    return (await req("/keys")).json();
  },

  async createKey(key: NewKey): Promise<void> {
    await req("/keys", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(key),
    });
  },

  async deleteKey(access: string): Promise<void> {
    await req(`/keys/${encodeURIComponent(access)}`, { method: "DELETE" });
  },

  // --- Phase 2: object metadata, versions, integrity, sharing ---

  async getMeta(bucket: string, key: string): Promise<ObjectMeta> {
    return (await req(`/meta/${encodeURIComponent(bucket)}/${encodeKey(key)}`)).json();
  },

  async setMeta(bucket: string, key: string, meta: ObjectMeta): Promise<void> {
    await req(`/meta/${encodeURIComponent(bucket)}/${encodeKey(key)}`, {
      method: "PUT",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(meta),
    });
  },

  async restore(bucket: string, key: string, objectId: string): Promise<void> {
    await req(`/restore/${encodeURIComponent(bucket)}/${encodeKey(key)}`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ object_id: objectId }),
    });
  },

  async diff(bucket: string, key: string, a: string, b: string): Promise<DiffResult> {
    const p = new URLSearchParams({ a, b }).toString();
    return (await req(`/diff/${encodeURIComponent(bucket)}/${encodeKey(key)}?${p}`)).json();
  },

  async verify(bucket: string, key: string): Promise<{ ok: boolean }> {
    return (
      await req(`/verify/${encodeURIComponent(bucket)}/${encodeKey(key)}`, {
        method: "POST",
      })
    ).json();
  },

  async presign(bucket: string, key: string, expiresSecs: number): Promise<{ url: string }> {
    return (
      await req(`/presign/${encodeURIComponent(bucket)}/${encodeKey(key)}`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ expires_secs: expiresSecs }),
      })
    ).json();
  },

  // --- Phase 3: import, zip ---

  async importUrl(bucket: string, url: string, key: string): Promise<{ object_id: string }> {
    return (
      await req(`/pots/${encodeURIComponent(bucket)}/import`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ url, key }),
      })
    ).json();
  },

  async zip(bucket: string, keys: string[]): Promise<Blob> {
    const q = keys.map(encodeURIComponent).join(",");
    return (await req(`/pots/${encodeURIComponent(bucket)}/zip?keys=${q}`)).blob();
  },

  // --- Phase 5: similarity ---

  async similar(hash: string): Promise<SearchHit[]> {
    const res = await req(`/similar/${encodeURIComponent(hash)}`, { method: "POST" });
    return res.status === 501 ? [] : res.json();
  },

  // --- Phase 6: webhooks, health ---

  async listWebhooks(): Promise<Webhook[]> {
    return (await req("/webhooks")).json();
  },

  async saveWebhook(url: string, events: string[]): Promise<Webhook> {
    return (
      await req("/webhooks", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ url, events }),
      })
    ).json();
  },

  async deleteWebhook(id: string): Promise<void> {
    await req(`/webhooks/${encodeURIComponent(id)}`, { method: "DELETE" });
  },

  async health(): Promise<Health> {
    return (await req("/health")).json();
  },
};

export interface UploadHandle {
  promise: Promise<{ object_id: string }>;
  cancel: () => void;
}

/** XHR-based upload exposing real progress + cancel, unlike fetch. */
export function uploadWithProgress(
  bucket: string,
  key: string,
  file: File,
  onProgress: (loaded: number, total: number) => void,
): UploadHandle {
  const xhr = new XMLHttpRequest();
  const promise = new Promise<{ object_id: string }>((resolve, reject) => {
    xhr.open("PUT", BASE + `/objects/${encodeURIComponent(bucket)}/${encodeKey(key)}`);
    const c = loadCreds();
    if (c) xhr.setRequestHeader("Authorization", "Basic " + btoa(`${c.access}:${c.secret}`));
    xhr.setRequestHeader("Content-Type", file.type || "application/octet-stream");
    xhr.upload.onprogress = (e) => {
      if (e.lengthComputable) onProgress(e.loaded, e.total);
    };
    xhr.onload = () => {
      if (xhr.status >= 200 && xhr.status < 300) {
        try {
          resolve(JSON.parse(xhr.responseText) as { object_id: string });
        } catch {
          resolve({ object_id: "" });
        }
      } else {
        reject(new ApiError(xhr.status, xhr.responseText || xhr.statusText));
      }
    };
    xhr.onerror = () => reject(new ApiError(0, "network error"));
    xhr.onabort = () => reject(new ApiError(0, "aborted"));
    xhr.send(file);
  });
  return { promise, cancel: () => xhr.abort() };
}

async function ops(path: string, fromB: string, fromK: string, toB: string, toK: string) {
  await req(path, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({
      from_bucket: fromB,
      from_key: fromK,
      to_bucket: toB,
      to_key: toK,
    }),
  });
}

/** Immutable public URL for an object by its content hash (caches forever). */
export function cdnUrl(objectId: string): string {
  return `${CDN}/cdn/${objectId}`;
}

/** Friendly public URL, only resolves if the bucket is public. */
export function publicUrl(bucket: string, key: string): string {
  const k = key.split("/").map(encodeURIComponent).join("/");
  return `${CDN}/public/${encodeURIComponent(bucket)}/${k}`;
}

/** Trigger a browser download for an object. */
export async function downloadToDisk(bucket: string, key: string) {
  const blob = await api.download(bucket, key);
  const url = URL.createObjectURL(blob);
  const a = document.createElement("a");
  a.href = url;
  a.download = key.split("/").pop() ?? "download";
  a.click();
  URL.revokeObjectURL(url);
}
