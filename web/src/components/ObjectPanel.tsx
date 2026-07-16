import { useEffect, useState } from "react";
import { useNavigate } from "react-router-dom";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import {
  Copy,
  Download,
  FolderInput,
  GitCompare,
  Pencil,
  Share2,
  ShieldCheck,
  Sparkles,
  Star,
  Trash2,
  X,
} from "lucide-react";
import { api, cdnUrl, downloadToDisk, type ObjectMeta, type SearchHit } from "@/lib/api";
import { humanSize, shortHash } from "@/lib/format";
import { useToast } from "@/lib/toast";
import { useDialogs } from "@/lib/dialogs";
import { cn } from "@/lib/cn";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Badge } from "@/components/ui/badge";
import { ShareDialog } from "@/components/ShareDialog";

const EMPTY_META: ObjectMeta = { tags: {}, note: "", favorite: false, locked_until: null };

export function ObjectPanel({
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
  const qc = useQueryClient();
  const navigate = useNavigate();
  const { prompt, confirm } = useDialogs();
  const [sharing, setSharing] = useState(false);
  const [diffAgainst, setDiffAgainst] = useState<string | null>(null);
  const [similar, setSimilar] = useState<SearchHit[] | null>(null);

  const manifest = useQuery({
    queryKey: ["manifest", bucket, objectKey],
    queryFn: () => api.manifest(bucket, objectKey),
  });
  const history = useQuery({
    queryKey: ["history", bucket, objectKey],
    queryFn: () => api.history(bucket, objectKey),
  });
  const metaQuery = useQuery({
    queryKey: ["meta", bucket, objectKey],
    queryFn: () => api.getMeta(bucket, objectKey).catch(() => EMPTY_META),
  });
  const m = manifest.data;

  const ct = m?.original.content_type ?? "";
  const isImage = ct.startsWith("image/");
  const isVideo = ct.startsWith("video/");
  const isAudio = ct.startsWith("audio/");
  const isPdf = ct === "application/pdf";
  const isText =
    ct.startsWith("text/") ||
    ct.includes("json") ||
    ct.includes("xml") ||
    ct.includes("javascript") ||
    ct.includes("markdown");

  const text = useQuery({
    queryKey: ["preview", bucket, objectKey],
    queryFn: async () => (await api.download(bucket, objectKey)).text(),
    enabled: isText,
  });

  // Editable meta, seeded from the server copy.
  const [meta, setMeta] = useState<ObjectMeta>(EMPTY_META);
  const [tagInput, setTagInput] = useState("");
  useEffect(() => {
    if (metaQuery.data) setMeta(metaQuery.data);
  }, [metaQuery.data]);

  const saveMeta = useMutation({
    mutationFn: (mm: ObjectMeta) => api.setMeta(bucket, objectKey, mm),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ["meta", bucket, objectKey] });
      toast("Saved", "success");
    },
    onError: () => toast("Save failed", "error"),
  });

  const diff = useQuery({
    queryKey: ["diff", bucket, objectKey, m?.object_id, diffAgainst],
    queryFn: () => api.diff(bucket, objectKey, m!.object_id, diffAgainst!),
    enabled: !!diffAgainst && !!m,
  });

  function addTag() {
    const t = tagInput.trim();
    if (!t) return;
    setMeta((mm) => ({ ...mm, tags: { ...mm.tags, [t]: "" } }));
    setTagInput("");
  }
  function removeTag(t: string) {
    setMeta((mm) => {
      const tags = { ...mm.tags };
      delete tags[t];
      return { ...mm, tags };
    });
  }

  async function verify() {
    try {
      const r = await api.verify(bucket, objectKey);
      toast(r.ok ? "Integrity OK" : "Integrity failed", r.ok ? "success" : "error");
    } catch {
      toast("Verify failed", "error");
    }
  }

  async function restore(id: string) {
    try {
      await api.restore(bucket, objectKey, id);
      qc.invalidateQueries({ queryKey: ["history", bucket, objectKey] });
      qc.invalidateQueries({ queryKey: ["manifest", bucket, objectKey] });
      toast("Restored version", "success");
      onChanged();
    } catch {
      toast("Restore failed", "error");
    }
  }

  async function findSimilar() {
    if (!m) return;
    try {
      setSimilar(await api.similar(m.object_id));
    } catch {
      toast("Similarity search failed", "error");
    }
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

  function toggleFavorite() {
    const next = { ...meta, favorite: !meta.favorite };
    setMeta(next);
    saveMeta.mutate(next);
  }

  const tagKeys = Object.keys(meta.tags);

  return (
    <aside className="flex w-96 shrink-0 flex-col border-l border-border bg-panel">
      <div className="flex h-14 items-center justify-between border-b border-border px-4">
        <div className="flex min-w-0 items-center gap-2">
          <button
            onClick={toggleFavorite}
            title="Favorite"
            className={cn("shrink-0", meta.favorite ? "text-warn" : "text-faint hover:text-text")}
          >
            <Star className={cn("size-4", meta.favorite && "fill-current")} />
          </button>
          <span className="truncate text-sm font-medium">{objectKey}</span>
        </div>
        <button onClick={onClose} className="text-muted transition-colors hover:text-text">
          <X className="size-4" />
        </button>
      </div>

      <div className="min-h-0 flex-1 space-y-5 overflow-y-auto p-4">
        {/* Rich preview */}
        {m && isImage && (
          <img
            src={cdnUrl(m.object_id)}
            alt={objectKey}
            className="max-h-56 w-full rounded-lg border border-border object-contain"
          />
        )}
        {m && isVideo && (
          <video src={cdnUrl(m.object_id)} controls className="max-h-56 w-full rounded-lg border border-border" />
        )}
        {m && isAudio && <audio src={cdnUrl(m.object_id)} controls className="w-full" />}
        {m && isPdf && (
          <embed src={cdnUrl(m.object_id)} type="application/pdf" className="h-64 w-full rounded-lg border border-border" />
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
              <Badge tone={m.storage.fidelity === "exact" ? "ok" : "warn"}>{m.storage.fidelity}</Badge>
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

        {/* Tags editor */}
        <div>
          <div className="mb-2 text-[11px] uppercase tracking-wider text-faint">Tags</div>
          <div className="mb-2 flex flex-wrap gap-1.5">
            {tagKeys.length ? (
              tagKeys.map((t) => (
                <span
                  key={t}
                  className="inline-flex items-center gap-1 rounded-full bg-accent/15 px-2 py-0.5 text-[11px] text-accent"
                >
                  {t}
                  <button onClick={() => removeTag(t)} className="hover:text-text">
                    <X className="size-3" />
                  </button>
                </span>
              ))
            ) : (
              <span className="text-xs text-faint">No tags.</span>
            )}
          </div>
          <div className="flex gap-2">
            <Input
              value={tagInput}
              onChange={(e) => setTagInput(e.target.value)}
              onKeyDown={(e) => e.key === "Enter" && (e.preventDefault(), addTag())}
              placeholder="Add tag…"
              className="text-xs"
            />
            <Button variant="outline" onClick={addTag}>
              Add
            </Button>
          </div>
        </div>

        {/* Note */}
        <div>
          <div className="mb-2 text-[11px] uppercase tracking-wider text-faint">Note</div>
          <textarea
            value={meta.note}
            onChange={(e) => setMeta((mm) => ({ ...mm, note: e.target.value }))}
            placeholder="Add a note…"
            rows={3}
            className="w-full resize-y rounded-md border border-border bg-panel px-3 py-2 text-xs text-text outline-none transition-colors placeholder:text-faint focus:border-accent"
          />
        </div>

        <Button className="w-full" onClick={() => saveMeta.mutate(meta)} disabled={saveMeta.isPending}>
          {saveMeta.isPending ? "Saving…" : "Save metadata"}
        </Button>

        {/* Actions */}
        <div className="grid grid-cols-2 gap-2">
          <Button variant="outline" onClick={() => setSharing(true)}>
            <Share2 className="size-4" /> Share
          </Button>
          <Button variant="outline" onClick={verify}>
            <ShieldCheck className="size-4" /> Verify
          </Button>
          <Button variant="outline" onClick={findSimilar} className="col-span-2">
            <Sparkles className="size-4" /> Find similar
          </Button>
        </div>

        {similar && (
          <div>
            <div className="mb-2 text-[11px] uppercase tracking-wider text-faint">Similar</div>
            {similar.length ? (
              <ol className="space-y-1">
                {similar.map((h) => (
                  <li key={h.id}>
                    <button
                      onClick={() =>
                        h.pot && h.key && navigate(`/p/${encodeURIComponent(h.pot)}?key=${encodeURIComponent(h.key)}`)
                      }
                      className="flex w-full items-center justify-between rounded bg-elevated/50 px-2 py-1 text-xs hover:bg-elevated"
                    >
                      <span className="truncate text-muted">{h.key ?? shortHash(h.id)}</span>
                      <span className="text-faint">{h.score.toFixed(3)}</span>
                    </button>
                  </li>
                ))}
              </ol>
            ) : (
              <p className="text-xs text-faint">No similar objects.</p>
            )}
          </div>
        )}

        {/* Versions */}
        <div>
          <div className="mb-2 text-[11px] uppercase tracking-wider text-faint">
            Versions ({history.data?.length ?? 0})
          </div>
          <ol className="space-y-1">
            {history.data?.map((id, i) => {
              const isCurrent = id === m?.object_id;
              return (
                <li key={id} className="rounded bg-elevated/50 px-2 py-1.5 text-xs">
                  <div className="flex items-center justify-between gap-2">
                    <span className="font-mono text-muted">{shortHash(id)}</span>
                    <span className="flex items-center gap-2">
                      <span className="text-faint">v{i + 1}{isCurrent ? " ·now" : ""}</span>
                      {!isCurrent && (
                        <>
                          <button
                            onClick={() => setDiffAgainst(diffAgainst === id ? null : id)}
                            title="Diff vs current"
                            className="text-muted hover:text-text"
                          >
                            <GitCompare className="size-3.5" />
                          </button>
                          <button onClick={() => restore(id)} className="text-accent hover:underline">
                            Restore
                          </button>
                        </>
                      )}
                    </span>
                  </div>
                  {diffAgainst === id && (
                    <div className="mt-1.5 border-t border-border pt-1.5 text-[11px]">
                      {diff.isLoading ? (
                        <span className="text-faint">Diffing…</span>
                      ) : diff.data ? (
                        <div className="flex gap-3">
                          <span className="text-ok">+{diff.data.added.length}</span>
                          <span className="text-danger">−{diff.data.removed.length}</span>
                          <span className="text-muted">={diff.data.shared.length} shared</span>
                        </div>
                      ) : (
                        <span className="text-faint">No diff.</span>
                      )}
                    </div>
                  )}
                </li>
              );
            })}
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

      {sharing && m && (
        <ShareDialog
          bucket={bucket}
          objectKey={objectKey}
          objectId={m.object_id}
          isPublic={isPublic}
          onClose={() => setSharing(false)}
        />
      )}
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
