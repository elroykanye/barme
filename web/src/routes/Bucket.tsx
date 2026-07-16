import { useRef, useState } from "react";
import { useParams } from "react-router-dom";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Download, Globe, Lock, Trash2, Upload, X } from "lucide-react";
import { api, downloadToDisk } from "@/lib/api";
import { humanSize, shortHash } from "@/lib/format";
import { cn } from "@/lib/cn";
import { Button } from "@/components/ui/button";
import { Badge } from "@/components/ui/badge";

export function BucketView() {
  const { bucket = "" } = useParams();
  const qc = useQueryClient();
  const fileRef = useRef<HTMLInputElement>(null);
  const [selected, setSelected] = useState<string | null>(null);
  const [dragging, setDragging] = useState(false);

  const objects = useQuery({
    queryKey: ["objects", bucket],
    queryFn: () => api.listObjects(bucket),
  });
  const buckets = useQuery({ queryKey: ["buckets"], queryFn: api.listBuckets });
  const info = buckets.data?.find((b) => b.name === bucket);

  const upload = useMutation({
    mutationFn: async (files: FileList) => {
      for (const f of Array.from(files)) await api.upload(bucket, f.name, f);
    },
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ["objects", bucket] });
      qc.invalidateQueries({ queryKey: ["buckets"] });
    },
  });
  const remove = useMutation({
    mutationFn: (key: string) => api.remove(bucket, key),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ["objects", bucket] });
      qc.invalidateQueries({ queryKey: ["buckets"] });
      setSelected(null);
    },
  });
  const toggle = useMutation({
    mutationFn: (pub: boolean) => api.setVisibility(bucket, pub),
    onSuccess: () => qc.invalidateQueries({ queryKey: ["buckets"] }),
  });

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
        <div className="mb-5 flex items-center justify-between">
          <div className="flex items-center gap-3">
            <h1 className="text-lg font-semibold tracking-tight">{bucket}</h1>
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
          <div className="flex items-center gap-2">
            {info && (
              <Button variant="outline" onClick={() => toggle.mutate(!info.public_read)}>
                {info.public_read ? "Make private" : "Make public"}
              </Button>
            )}
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

        <div
          className={cn(
            "overflow-hidden rounded-xl border transition-colors",
            dragging ? "border-accent bg-accent/5" : "border-border",
          )}
        >
          {objects.isLoading ? (
            <p className="p-6 text-sm text-muted">Loading…</p>
          ) : !objects.data?.length ? (
            <div className="p-12 text-center text-sm text-muted">
              Empty. Drop files here, or use Upload.
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
                {objects.data.map((o) => (
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
          onClose={() => setSelected(null)}
          onDelete={() => remove.mutate(selected)}
        />
      )}
    </div>
  );
}

function ObjectPanel({
  bucket,
  objectKey,
  onClose,
  onDelete,
}: {
  bucket: string;
  objectKey: string;
  onClose: () => void;
  onDelete: () => void;
}) {
  const manifest = useQuery({
    queryKey: ["manifest", bucket, objectKey],
    queryFn: () => api.manifest(bucket, objectKey),
  });
  const history = useQuery({
    queryKey: ["history", bucket, objectKey],
    queryFn: () => api.history(bucket, objectKey),
  });
  const m = manifest.data;

  return (
    <aside className="flex w-80 shrink-0 flex-col border-l border-border bg-panel">
      <div className="flex h-14 items-center justify-between border-b border-border px-4">
        <span className="truncate text-sm font-medium">{objectKey}</span>
        <button onClick={onClose} className="text-muted transition-colors hover:text-text">
          <X className="size-4" />
        </button>
      </div>

      <div className="min-h-0 flex-1 space-y-5 overflow-y-auto p-4">
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

      <div className="flex gap-2 border-t border-border p-4">
        <Button variant="outline" className="flex-1" onClick={() => void downloadToDisk(bucket, objectKey)}>
          <Download className="size-4" />
          Download
        </Button>
        <Button variant="danger" onClick={onDelete}>
          <Trash2 className="size-4" />
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
