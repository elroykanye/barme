import { useRef, useState } from "react";
import { Link, useNavigate, useParams } from "react-router-dom";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import {
  Copy,
  Download,
  FolderInput,
  Globe,
  Link2,
  Lock,
  Pencil,
  Search,
  Settings,
  Trash2,
  Upload,
  X,
} from "lucide-react";
import { api, cdnUrl, downloadToDisk, publicUrl } from "@/lib/api";
import { humanSize, shortHash } from "@/lib/format";
import { copyText } from "@/lib/clipboard";
import { useToast } from "@/lib/toast";
import { useDialogs } from "@/lib/dialogs";
import { cn } from "@/lib/cn";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Badge } from "@/components/ui/badge";

export function BucketView() {
  const { bucket = "" } = useParams();
  const navigate = useNavigate();
  const toast = useToast();
  const { prompt, confirm } = useDialogs();
  const qc = useQueryClient();
  const fileRef = useRef<HTMLInputElement>(null);
  const [selected, setSelected] = useState<string | null>(null);
  const [dragging, setDragging] = useState(false);
  const [filter, setFilter] = useState("");

  const objects = useQuery({
    queryKey: ["objects", bucket],
    queryFn: () => api.listObjects(bucket),
  });
  const buckets = useQuery({ queryKey: ["buckets"], queryFn: api.listBuckets });
  const info = buckets.data?.find((b) => b.name === bucket);

  const refreshAll = () => {
    qc.invalidateQueries({ queryKey: ["objects", bucket] });
    qc.invalidateQueries({ queryKey: ["buckets"] });
    qc.invalidateQueries({ queryKey: ["stats"] });
  };

  const upload = useMutation({
    mutationFn: async (files: FileList) => {
      for (const f of Array.from(files)) await api.upload(bucket, f.name, f);
    },
    onSuccess: () => {
      refreshAll();
      toast("Uploaded", "success");
    },
    onError: () => toast("Upload failed", "error"),
  });
  const toggle = useMutation({
    mutationFn: (pub: boolean) => api.setVisibility(bucket, pub),
    onSuccess: () => qc.invalidateQueries({ queryKey: ["buckets"] }),
  });

  async function renameBucket() {
    const name = (
      await prompt({ title: "Rename pot", label: "New name", defaultValue: bucket })
    )?.trim();
    if (!name || name === bucket) return;
    try {
      await api.renameBucket(bucket, name);
      qc.invalidateQueries({ queryKey: ["buckets"] });
      toast("Pot renamed", "success");
      navigate(`/p/${encodeURIComponent(name)}`);
    } catch {
      toast("Rename failed (name taken?)", "error");
    }
  }

  async function deleteBucket() {
    const ok = await confirm({
      title: "Delete pot",
      message: `Delete pot "${bucket}" and all its objects? This cannot be undone.`,
      confirmLabel: "Delete",
      danger: true,
    });
    if (!ok) return;
    try {
      await api.deleteBucket(bucket);
      qc.invalidateQueries({ queryKey: ["buckets"] });
      toast("Pot deleted", "success");
      navigate("/");
    } catch {
      toast("Delete failed", "error");
    }
  }

  const rows = (objects.data ?? []).filter((o) =>
    o.key.toLowerCase().includes(filter.toLowerCase()),
  );

  return (
    <div className="flex h-full">
      <div
        className="min-w-0 flex-1 overflow-y-auto p-6"
        onDragOver={(e) => {
          e.preventDefault();
          setDragging(true);
        }}
        onDragLeave={() => setDragging(false)}
        onDrop={(e) => {
          e.preventDefault();
          setDragging(false);
          if (e.dataTransfer.files.length) upload.mutate(e.dataTransfer.files);
        }}
      >
        <div className="mb-4 flex items-center justify-between gap-4">
          <div className="flex min-w-0 items-center gap-3">
            <h1 className="truncate text-lg font-semibold tracking-tight">{bucket}</h1>
            {info &&
              (info.public_read ? (
                <Badge tone="ok">
                  <Globe className="size-3" />
                  public
                </Badge>
              ) : (
                <Badge>
                  <Lock className="size-3" />
                  private
                </Badge>
              ))}
          </div>
          <div className="flex shrink-0 items-center gap-2">
            {info && (
              <Button variant="ghost" onClick={() => toggle.mutate(!info.public_read)}>
                {info.public_read ? "Make private" : "Make public"}
              </Button>
            )}
            <Link to={`/p/${encodeURIComponent(bucket)}/settings`}>
              <Button variant="ghost" title="Pot settings">
                <Settings className="size-4" />
              </Button>
            </Link>
            <Button variant="ghost" onClick={renameBucket} title="Rename pot">
              <Pencil className="size-4" />
            </Button>
            <Button variant="danger" onClick={deleteBucket} title="Delete pot">
              <Trash2 className="size-4" />
            </Button>
            <Button onClick={() => fileRef.current?.click()}>
              <Upload className="size-4" />
              Upload
            </Button>
            <input
              ref={fileRef}
              type="file"
              multiple
              hidden
              onChange={(e) => e.target.files && upload.mutate(e.target.files)}
            />
          </div>
        </div>

        <div className="relative mb-4 max-w-xs">
          <Search className="pointer-events-none absolute left-3 top-1/2 size-4 -translate-y-1/2 text-faint" />
          <Input
            value={filter}
            onChange={(e) => setFilter(e.target.value)}
            placeholder="Filter by key…"
            className="pl-9"
          />
        </div>

        <div
          className={cn(
            "overflow-hidden rounded-xl border transition-colors",
            dragging ? "border-accent bg-accent/5" : "border-border",
          )}
        >
          {objects.isLoading ? (
            <p className="p-6 text-sm text-muted">Loading…</p>
          ) : !rows.length ? (
            <div className="p-12 text-center text-sm text-muted">
              {filter ? "No keys match." : "Empty. Drop files here, or use Upload."}
            </div>
          ) : (
            <table className="w-full text-sm">
              <thead>
                <tr className="border-b border-border text-left text-[11px] uppercase tracking-wider text-faint">
                  <th className="px-4 py-2.5 font-medium">Key</th>
                  <th className="px-4 py-2.5 font-medium">Size</th>
                  <th className="px-4 py-2.5 font-medium">Versions</th>
                  <th />
                </tr>
              </thead>
              <tbody>
                {rows.map((o) => (
                  <tr
                    key={o.key}
                    onClick={() => setSelected(o.key)}
                    className={cn(
                      "cursor-pointer border-b border-border/60 last:border-0 hover:bg-elevated/50",
                      selected === o.key && "bg-elevated",
                    )}
                  >
                    <td className="px-4 py-2.5">{o.key}</td>
                    <td className="px-4 py-2.5 text-muted">{humanSize(o.size)}</td>
                    <td className="px-4 py-2.5 text-muted">{o.versions}</td>
                    <td className="px-4 py-2.5 text-right">
                      <button
                        onClick={(e) => {
                          e.stopPropagation();
                          void downloadToDisk(bucket, o.key);
                        }}
                        className="text-muted transition-colors hover:text-text"
                        title="Download"
                      >
                        <Download className="size-4" />
                      </button>
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          )}
        </div>
      </div>

      {selected && (
        <ObjectPanel
          bucket={bucket}
          objectKey={selected}
          isPublic={info?.public_read ?? false}
          onClose={() => setSelected(null)}
          onChanged={refreshAll}
        />
      )}
    </div>
  );
}

function ObjectPanel({
  bucket,
  objectKey,
  isPublic,
  onClose,
  onChanged,
}: {
  bucket: string;
  objectKey: string;
  isPublic: boolean;
  onClose: () => void;
  onChanged: () => void;
}) {
  const toast = useToast();
  const { prompt, confirm } = useDialogs();
  const manifest = useQuery({
    queryKey: ["manifest", bucket, objectKey],
    queryFn: () => api.manifest(bucket, objectKey),
  });
  const history = useQuery({
    queryKey: ["history", bucket, objectKey],
    queryFn: () => api.history(bucket, objectKey),
  });
  const m = manifest.data;

  const ct = m?.original.content_type ?? "";
  const isImage = ct.startsWith("image/");
  const isText = ct.startsWith("text/") || ct.includes("json") || ct.includes("xml");

  const text = useQuery({
    queryKey: ["preview", bucket, objectKey],
    queryFn: async () => (await api.download(bucket, objectKey)).text(),
    enabled: isText,
  });

  function copy(url: string, label: string) {
    copyText(url)
      .then(() => toast(`${label} copied`, "success"))
      .catch(() => toast("Copy failed", "error"));
  }

  async function rename() {
    const to = (
      await prompt({ title: "Rename object", label: "New key", defaultValue: objectKey })
    )?.trim();
    if (!to || to === objectKey) return;
    await api.moveObject(bucket, objectKey, bucket, to);
    toast("Renamed", "success");
    onChanged();
    onClose();
  }

  async function move(copyInstead: boolean) {
    const dest = (
      await prompt({
        title: copyInstead ? "Copy object" : "Move object",
        label: "Destination (pot/key)",
        defaultValue: `${bucket}/${objectKey}`,
      })
    )?.trim();
    if (!dest) return;
    const i = dest.indexOf("/");
    if (i < 1) {
      toast("Use pot/key", "error");
      return;
    }
    const tb = dest.slice(0, i);
    const tk = dest.slice(i + 1);
    const fn = copyInstead ? api.copyObject : api.moveObject;
    await fn(bucket, objectKey, tb, tk);
    toast(copyInstead ? "Copied" : "Moved", "success");
    onChanged();
    if (!copyInstead) onClose();
  }

  async function del() {
    const ok = await confirm({
      title: "Delete object",
      message: `Delete "${objectKey}"?`,
      confirmLabel: "Delete",
      danger: true,
    });
    if (!ok) return;
    await api.remove(bucket, objectKey);
    toast("Deleted", "success");
    onChanged();
    onClose();
  }

  return (
    <aside className="flex w-96 shrink-0 flex-col border-l border-border bg-panel">
      <div className="flex h-14 items-center justify-between border-b border-border px-4">
        <span className="truncate text-sm font-medium">{objectKey}</span>
        <button onClick={onClose} className="text-muted transition-colors hover:text-text">
          <X className="size-4" />
        </button>
      </div>

      <div className="min-h-0 flex-1 space-y-5 overflow-y-auto p-4">
        {isImage && m && (
          <img
            src={cdnUrl(m.object_id)}
            alt={objectKey}
            className="max-h-56 w-full rounded-lg border border-border object-contain"
          />
        )}
        {isText && text.data !== undefined && (
          <pre className="max-h-56 overflow-auto rounded-lg border border-border bg-bg p-3 text-xs text-muted">
            {text.data.slice(0, 4000)}
          </pre>
        )}

        {m && (
          <>
            <div className="flex flex-wrap gap-1.5">
              <Badge>{m.storage.route}</Badge>
              <Badge tone={m.storage.fidelity === "exact" ? "ok" : "warn"}>
                {m.storage.fidelity}
              </Badge>
              <Badge tone="accent">{m.storage.codec}</Badge>
            </div>
            <dl className="space-y-2 text-xs">
              <Row k="Original" v={humanSize(m.original.size_bytes)} />
              <Row k="Stored" v={humanSize(m.storage.stored_size_bytes)} />
              <Row k="Type" v={m.original.content_type} />
              <Row k="Chunks" v={String(m.chunking.chunks.length)} />
              <Row k="Object id" v={shortHash(m.object_id)} mono />
            </dl>

            <div className="space-y-2">
              <div className="text-[11px] uppercase tracking-wider text-faint">Share</div>
              <button
                onClick={() => copy(cdnUrl(m.object_id), "Immutable link")}
                className="flex w-full items-center gap-2 rounded-md border border-border px-3 py-2 text-left text-xs text-muted hover:bg-elevated hover:text-text"
              >
                <Link2 className="size-3.5" /> Copy immutable CDN link
              </button>
              {isPublic && (
                <button
                  onClick={() => copy(publicUrl(bucket, objectKey), "Public link")}
                  className="flex w-full items-center gap-2 rounded-md border border-border px-3 py-2 text-left text-xs text-muted hover:bg-elevated hover:text-text"
                >
                  <Globe className="size-3.5" /> Copy public link
                </button>
              )}
            </div>
          </>
        )}

        <div>
          <div className="mb-2 text-[11px] uppercase tracking-wider text-faint">
            Versions ({history.data?.length ?? 0})
          </div>
          <ol className="space-y-1">
            {history.data?.map((id, i) => (
              <li
                key={id}
                className="flex items-center justify-between rounded bg-elevated/50 px-2 py-1 text-xs"
              >
                <span className="font-mono text-muted">{shortHash(id)}</span>
                <span className="text-faint">v{i + 1}</span>
              </li>
            ))}
          </ol>
        </div>
      </div>

      <div className="space-y-2 border-t border-border p-4">
        <div className="flex gap-2">
          <Button variant="outline" className="flex-1" onClick={() => void downloadToDisk(bucket, objectKey)}>
            <Download className="size-4" />
            Download
          </Button>
          <Button variant="outline" onClick={rename} title="Rename">
            <Pencil className="size-4" />
          </Button>
          <Button variant="outline" onClick={() => move(false)} title="Move">
            <FolderInput className="size-4" />
          </Button>
          <Button variant="outline" onClick={() => move(true)} title="Copy">
            <Copy className="size-4" />
          </Button>
        </div>
        <Button variant="danger" className="w-full" onClick={del}>
          <Trash2 className="size-4" />
          Delete
        </Button>
      </div>
    </aside>
  );
}

function Row({ k, v, mono }: { k: string; v: string; mono?: boolean }) {
  return (
    <div className="flex items-center justify-between gap-2">
      <dt className="text-faint">{k}</dt>
      <dd className={cn("truncate", mono && "font-mono")}>{v}</dd>
    </div>
  );
}
