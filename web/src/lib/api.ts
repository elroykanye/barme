// Thin client for the barme native API. Auth is Basic (owner access/secret),
// held in localStorage since this console runs on the owner's own machine.

const BASE = import.meta.env.VITE_BARME_API ?? "http://localhost:7373";

export type Creds = { access: string; secret: string };

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
    return (await req("/buckets")).json();
  },

  async listObjects(bucket: string): Promise<ObjectInfo[]> {
    return (await req(`/buckets/${encodeURIComponent(bucket)}/objects`)).json();
  },

  async setVisibility(bucket: string, publicRead: boolean): Promise<void> {
    await req(`/buckets/${encodeURIComponent(bucket)}/visibility`, {
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
};

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
