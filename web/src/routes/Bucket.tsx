import { useEffect, useMemo, useRef, useState } from "react";
import { Link, useNavigate, useParams, useSearchParams } from "react-router-dom";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import {
  ArrowDown,
  ArrowUp,
  ChevronRight,
  Download,
  FileArchive,
  Folder,
  Globe,
  LayoutGrid,
  Link as LinkIcon,
  List,
  Lock,
  Pencil,
  Search,
  Settings,
  Tag,
  Trash2,
  Upload,
  X,
} from "lucide-react";
import { api, downloadToDisk, publicUrl, type ObjectInfo } from "@/lib/api";
import { humanSize } from "@/lib/format";
import { useToast } from "@/lib/toast";
import { useDialogs } from "@/lib/dialogs";
import { useUploadManager } from "@/lib/uploads";
import { cn } from "@/lib/cn";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Badge } from "@/components/ui/badge";
import { Checkbox } from "@/components/ui/checkbox";
import { UploadPanel } from "@/components/UploadPanel";
import { ObjectPanel } from "@/components/ObjectPanel";

type SortKey = "key" | "size" | "versions";
type View = "list" | "grid";

const IMG_RE = /\.(png|jpe?g|gif|webp|avif|svg|bmp)$/i;

export function BucketView() {
  const { bucket = "" } = useParams();
  const navigate = useNavigate();
  const toast = useToast();
  const { prompt, confirm } = useDialogs();
  const qc = useQueryClient();
  const fileRef = useRef<HTMLInputElement>(null);
  const folderRef = useRef<HTMLInputElement>(null);

  const [searchParams, setSearchParams] = useSearchParams();
  const [selectedKey, setSelectedKey] = useState<string | null>(null);
  const [checked, setChecked] = useState<Set<string>>(new Set());
  const [dragging, setDragging] = useState(false);
  const [filter, setFilter] = useState("");
  const [prefix, setPrefix] = useState("");
  const [view, setView] = useState<View>("list");
  const [sort, setSort] = useState<{ key: SortKey; dir: 1 | -1 }>({ key: "key", dir: 1 });

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

  const uploads = useUploadManager(refreshAll);

  // webkitdirectory is non-standard; set it imperatively to stay type-clean.
  useEffect(() => {
    const el = folderRef.current;
    if (el) {
      el.setAttribute("webkitdirectory", "");
      el.setAttribute("directory", "");
    }
  }, []);

  // Deep-link support: /p/:bucket?key=... opens that object's panel.
  const keyParam = searchParams.get("key");
  useEffect(() => {
    if (keyParam) {
      setSelectedKey(keyParam);
      const p = keyParam.includes("/") ? keyParam.slice(0, keyParam.lastIndexOf("/") + 1) : "";
      setPrefix(p);
    }
  }, [keyParam]);

  function closePanel() {
    setSelectedKey(null);
    if (keyParam) {
      searchParams.delete("key");
      setSearchParams(searchParams, { replace: true });
    }
  }

  const toggle = useMutation({
    mutationFn: (pub: boolean) => api.setVisibility(bucket, pub),
    onSuccess: () => qc.invalidateQueries({ queryKey: ["buckets"] }),
  });

  function startUpload(files: FileList | File[], asFolder: boolean) {
    const list = Array.from(files).map((f) => ({
      file: f,
      // Folder uploads preserve their relative path as the key.
      key: prefix + (asFolder ? webkitPath(f) : f.name),
    }));
    if (list.length) uploads.start(bucket, list);
  }

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

  async function importUrl() {
    const url = (await prompt({ title: "Import from URL", label: "Source URL" }))?.trim();
    if (!url) return;
    const guess = prefix + (url.split("/").pop() || "imported");
    const key = (await prompt({ title: "Import from URL", label: "Store as key", defaultValue: guess }))?.trim();
    if (!key) return;
    try {
      await api.importUrl(bucket, url, key);
      toast("Imported", "success");
      refreshAll();
    } catch {
      toast("Import failed", "error");
    }
  }

  const all = objects.data ?? [];
  const filtering = filter.trim().length > 0;

  // Folder view: split immediate subfolders and files under the current prefix.
  const level = useMemo(() => {
    if (filtering) {
      const f = filter.toLowerCase();
      return {
        folders: [] as { name: string; count: number }[],
        files: all.filter((o) => o.key.toLowerCase().includes(f)),
      };
    }
    const folders = new Map<string, number>();
    const files: ObjectInfo[] = [];
    for (const o of all) {
      if (!o.key.startsWith(prefix)) continue;
      const rest = o.key.slice(prefix.length);
      const slash = rest.indexOf("/");
      if (slash === -1) files.push(o);
      else {
        const name = rest.slice(0, slash);
        folders.set(name, (folders.get(name) ?? 0) + 1);
      }
    }
    return {
      folders: [...folders.entries()].map(([name, count]) => ({ name, count })).sort((a, b) => a.name.localeCompare(b.name)),
      files,
    };
  }, [all, prefix, filter, filtering]);

  const files = useMemo(() => {
    const arr = [...level.files];
    arr.sort((a, b) => {
      let r = 0;
      if (sort.key === "key") r = a.key.localeCompare(b.key);
      else if (sort.key === "size") r = a.size - b.size;
      else r = a.versions - b.versions;
      return r * sort.dir;
    });
    return arr;
  }, [level.files, sort]);

  const crumbs = prefix ? prefix.replace(/\/$/, "").split("/") : [];
  const allChecked = files.length > 0 && files.every((f) => checked.has(f.key));

  function toggleSort(key: SortKey) {
    setSort((s) => (s.key === key ? { key, dir: (s.dir === 1 ? -1 : 1) as 1 | -1 } : { key, dir: 1 }));
  }
  function toggleCheck(key: string) {
    setChecked((s) => {
      const n = new Set(s);
      if (n.has(key)) n.delete(key);
      else n.add(key);
      return n;
    });
  }
  function toggleAll() {
    setChecked((s) => {
      if (files.every((f) => s.has(f.key))) return new Set();
      return new Set(files.map((f) => f.key));
    });
  }
  function enterFolder(name: string) {
    setPrefix((p) => p + name + "/");
    setChecked(new Set());
  }
  function goCrumb(i: number) {
    setPrefix(crumbs.slice(0, i + 1).join("/") + "/");
    setChecked(new Set());
  }

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
          if (e.dataTransfer.files.length) startUpload(e.dataTransfer.files, false);
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
            <Button variant="ghost" onClick={importUrl} title="Import from URL">
              <LinkIcon className="size-4" />
            </Button>
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
            <Button variant="outline" onClick={() => folderRef.current?.click()} title="Upload folder">
              <Folder className="size-4" />
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
              onChange={(e) => {
                if (e.target.files) startUpload(e.target.files, false);
                e.target.value = "";
              }}
            />
            <input
              ref={folderRef}
              type="file"
              multiple
              hidden
              onChange={(e) => {
                if (e.target.files) startUpload(e.target.files, true);
                e.target.value = "";
              }}
            />
          </div>
        </div>

        <div className="mb-4 flex items-center justify-between gap-3">
          <div className="relative max-w-xs flex-1">
            <Search className="pointer-events-none absolute left-3 top-1/2 size-4 -translate-y-1/2 text-faint" />
            <Input
              value={filter}
              onChange={(e) => setFilter(e.target.value)}
              placeholder="Filter by key…"
              className="pl-9"
            />
          </div>
          <div className="flex items-center gap-1 rounded-md border border-border p-0.5">
            <button
              onClick={() => setView("list")}
              className={cn("rounded p-1.5", view === "list" ? "bg-elevated text-text" : "text-muted")}
              title="List"
            >
              <List className="size-4" />
            </button>
            <button
              onClick={() => setView("grid")}
              className={cn("rounded p-1.5", view === "grid" ? "bg-elevated text-text" : "text-muted")}
              title="Gallery"
            >
              <LayoutGrid className="size-4" />
            </button>
          </div>
        </div>

        {!filtering && crumbs.length > 0 && (
          <div className="mb-3 flex flex-wrap items-center gap-1 text-sm">
            <button onClick={() => { setPrefix(""); setChecked(new Set()); }} className="text-muted hover:text-text">
              {bucket}
            </button>
            {crumbs.map((c, i) => (
              <span key={i} className="flex items-center gap-1">
                <ChevronRight className="size-3.5 text-faint" />
                <button onClick={() => goCrumb(i)} className="text-muted hover:text-text">
                  {c}
                </button>
              </span>
            ))}
          </div>
        )}

        {checked.size > 0 && (
          <BulkBar
            bucket={bucket}
            keys={[...checked]}
            onClear={() => setChecked(new Set())}
            onChanged={refreshAll}
          />
        )}

        <div
          className={cn(
            "overflow-hidden rounded-xl border transition-colors",
            dragging ? "border-accent bg-accent/5" : "border-border",
          )}
        >
          {objects.isLoading ? (
            <p className="p-6 text-sm text-muted">Loading…</p>
          ) : !files.length && !level.folders.length ? (
            <div className="p-12 text-center text-sm text-muted">
              {filtering ? "No keys match." : "Empty. Drop files here, or use Upload."}
            </div>
          ) : view === "grid" ? (
            <Grid
              bucket={bucket}
              folders={level.folders}
              files={files}
              selectedKey={selectedKey}
              onOpenFolder={enterFolder}
              onSelect={setSelectedKey}
            />
          ) : (
            <table className="w-full text-sm">
              <thead>
                <tr className="border-b border-border text-left text-[11px] uppercase tracking-wider text-faint">
                  <th className="w-8 px-4 py-2.5">
                    <Checkbox
                      checked={allChecked}
                      indeterminate={!allChecked && files.some((f) => checked.has(f.key))}
                      onChange={toggleAll}
                    />
                  </th>
                  <SortTh label="Key" k="key" sort={sort} onSort={toggleSort} />
                  <SortTh label="Size" k="size" sort={sort} onSort={toggleSort} />
                  <SortTh label="Versions" k="versions" sort={sort} onSort={toggleSort} />
                  <th />
                </tr>
              </thead>
              <tbody>
                {level.folders.map((f) => (
                  <tr
                    key={"d:" + f.name}
                    onClick={() => enterFolder(f.name)}
                    className="cursor-pointer border-b border-border/60 hover:bg-elevated/50"
                  >
                    <td className="px-4 py-2.5" />
                    <td className="px-4 py-2.5">
                      <span className="flex items-center gap-2">
                        <Folder className="size-4 text-accent" />
                        {f.name}
                      </span>
                    </td>
                    <td className="px-4 py-2.5 text-muted">—</td>
                    <td className="px-4 py-2.5 text-muted">{f.count} item{f.count === 1 ? "" : "s"}</td>
                    <td />
                  </tr>
                ))}
                {files.map((o) => (
                  <tr
                    key={o.key}
                    onClick={() => setSelectedKey(o.key)}
                    className={cn(
                      "cursor-pointer border-b border-border/60 last:border-0 hover:bg-elevated/50",
                      selectedKey === o.key && "bg-elevated",
                    )}
                  >
                    <td className="px-4 py-2.5" onClick={(e) => e.stopPropagation()}>
                      <Checkbox checked={checked.has(o.key)} onChange={() => toggleCheck(o.key)} />
                    </td>
                    <td className="px-4 py-2.5">{leaf(o.key)}</td>
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

      {selectedKey && (
        <ObjectPanel
          bucket={bucket}
          objectKey={selectedKey}
          isPublic={info?.public_read ?? false}
          onClose={closePanel}
          onChanged={refreshAll}
        />
      )}

      <UploadPanel items={uploads.items} onClear={uploads.clearFinished} />
    </div>
  );
}

function SortTh({
  label,
  k,
  sort,
  onSort,
}: {
  label: string;
  k: SortKey;
  sort: { key: SortKey; dir: 1 | -1 };
  onSort: (k: SortKey) => void;
}) {
  return (
    <th className="px-4 py-2.5 font-medium">
      <button onClick={() => onSort(k)} className="flex items-center gap-1 uppercase hover:text-text">
        {label}
        {sort.key === k &&
          (sort.dir === 1 ? <ArrowUp className="size-3" /> : <ArrowDown className="size-3" />)}
      </button>
    </th>
  );
}

function Grid({
  bucket,
  folders,
  files,
  selectedKey,
  onOpenFolder,
  onSelect,
}: {
  bucket: string;
  folders: { name: string; count: number }[];
  files: ObjectInfo[];
  selectedKey: string | null;
  onOpenFolder: (name: string) => void;
  onSelect: (key: string) => void;
}) {
  return (
    <div className="grid grid-cols-2 gap-3 p-3 sm:grid-cols-3 lg:grid-cols-4">
      {folders.map((f) => (
        <button
          key={"d:" + f.name}
          onClick={() => onOpenFolder(f.name)}
          className="flex flex-col items-center gap-2 rounded-lg border border-border p-4 hover:bg-elevated"
        >
          <Folder className="size-8 text-accent" />
          <span className="w-full truncate text-center text-xs">{f.name}</span>
        </button>
      ))}
      {files.map((o) => (
        <button
          key={o.key}
          onClick={() => onSelect(o.key)}
          className={cn(
            "group flex flex-col overflow-hidden rounded-lg border text-left",
            selectedKey === o.key ? "border-accent" : "border-border hover:border-accent/50",
          )}
        >
          <div className="flex aspect-square items-center justify-center bg-bg">
            {IMG_RE.test(o.key) ? (
              <img
                src={publicUrl(bucket, o.key)}
                alt={o.key}
                loading="lazy"
                className="size-full object-cover"
                onError={(e) => (e.currentTarget.style.visibility = "hidden")}
              />
            ) : (
              <FileArchive className="size-8 text-faint" />
            )}
          </div>
          <div className="border-t border-border px-2 py-1.5">
            <div className="truncate text-xs">{leaf(o.key)}</div>
            <div className="text-[10px] text-faint">{humanSize(o.size)}</div>
          </div>
        </button>
      ))}
    </div>
  );
}

function BulkBar({
  bucket,
  keys,
  onClear,
  onChanged,
}: {
  bucket: string;
  keys: string[];
  onClear: () => void;
  onChanged: () => void;
}) {
  const toast = useToast();
  const { prompt, confirm } = useDialogs();

  async function zip() {
    try {
      const blob = await api.zip(bucket, keys);
      const url = URL.createObjectURL(blob);
      const a = document.createElement("a");
      a.href = url;
      a.download = `${bucket}.zip`;
      a.click();
      URL.revokeObjectURL(url);
    } catch {
      toast("Zip failed", "error");
    }
  }
  async function downloadEach() {
    for (const k of keys) await downloadToDisk(bucket, k);
  }
  async function tagAll() {
    const tag = (await prompt({ title: "Tag selection", label: "Tag" }))?.trim();
    if (!tag) return;
    try {
      for (const k of keys) {
        const meta = await api.getMeta(bucket, k).catch(() => ({ tags: {}, note: "", favorite: false, locked_until: null }));
        await api.setMeta(bucket, k, { ...meta, tags: { ...meta.tags, [tag]: "" } });
      }
      toast("Tagged", "success");
    } catch {
      toast("Tagging failed", "error");
    }
  }
  async function del() {
    const ok = await confirm({
      title: "Delete objects",
      message: `Delete ${keys.length} object${keys.length === 1 ? "" : "s"}?`,
      confirmLabel: "Delete",
      danger: true,
    });
    if (!ok) return;
    for (const k of keys) await api.remove(bucket, k);
    toast("Deleted", "success");
    onClear();
    onChanged();
  }

  return (
    <div className="mb-3 flex items-center gap-2 rounded-lg border border-accent/40 bg-accent/5 px-3 py-2 text-sm">
      <span className="font-medium">{keys.length} selected</span>
      <div className="ml-auto flex items-center gap-2">
        <Button variant="outline" onClick={zip} title="Download as zip">
          <FileArchive className="size-4" /> Zip
        </Button>
        <Button variant="outline" onClick={downloadEach} title="Download each">
          <Download className="size-4" />
        </Button>
        <Button variant="outline" onClick={tagAll}>
          <Tag className="size-4" /> Tag
        </Button>
        <Button variant="danger" onClick={del}>
          <Trash2 className="size-4" /> Delete
        </Button>
        <button onClick={onClear} className="text-muted hover:text-text" title="Clear">
          <X className="size-4" />
        </button>
      </div>
    </div>
  );
}

function leaf(key: string): string {
  return key.split("/").pop() || key;
}

function webkitPath(f: File): string {
  const rel = (f as File & { webkitRelativePath?: string }).webkitRelativePath;
  return rel && rel.length ? rel : f.name;
}
