import type { ReactNode } from "react";
import { useQuery } from "@tanstack/react-query";
import { Link } from "react-router-dom";
import { Database, HardDrive, Layers, Package } from "lucide-react";
import { api } from "@/lib/api";
import { humanSize } from "@/lib/format";
import { Badge } from "@/components/ui/badge";

export function Home() {
  const buckets = useQuery({ queryKey: ["buckets"], queryFn: api.listBuckets });
  const stats = useQuery({ queryKey: ["stats"], queryFn: api.stats });

  const s = stats.data;
  const saved =
    s && s.logical_bytes > 0
      ? Math.max(0, 100 - (100 * s.physical_bytes) / s.logical_bytes)
      : 0;

  return (
    <div className="mx-auto h-full max-w-4xl overflow-y-auto p-8">
      <h1 className="mb-5 text-lg font-semibold tracking-tight">Overview</h1>

      <div className="mb-8 grid grid-cols-2 gap-3 lg:grid-cols-4">
        <Stat icon={<Package className="size-4" />} label="Objects" value={s ? String(s.objects) : "—"} />
        <Stat icon={<Database className="size-4" />} label="Pots" value={s ? String(s.buckets) : "—"} />
        <Stat
          icon={<HardDrive className="size-4" />}
          label="On disk"
          value={s ? humanSize(s.physical_bytes) : "—"}
          sub={s ? `${humanSize(s.logical_bytes)} logical` : undefined}
        />
        <Stat
          icon={<Layers className="size-4" />}
          label="Saved"
          value={s ? `${saved.toFixed(0)}%` : "—"}
          sub={s ? `${s.unique_chunks} chunks` : undefined}
          accent
        />
      </div>

      <h2 className="mb-3 text-sm font-medium text-muted">Pots</h2>
      {buckets.isLoading ? (
        <p className="text-sm text-muted">Loading…</p>
      ) : !buckets.data?.length ? (
        <div className="rounded-xl border border-dashed border-border p-12 text-center">
          <p className="text-sm text-muted">No pots yet.</p>
          <p className="mt-1 text-xs text-faint">
            Create one with the + in the sidebar, then upload a file.
          </p>
        </div>
      ) : (
        <div className="grid grid-cols-1 gap-3 sm:grid-cols-2 lg:grid-cols-3">
          {buckets.data.map((b) => (
            <Link
              key={b.name}
              to={`/p/${encodeURIComponent(b.name)}`}
              className="group rounded-xl border border-border bg-panel p-4 transition-colors hover:border-accent/50 hover:bg-elevated"
            >
              <div className="mb-3 flex items-center justify-between">
                <Database className="size-4 text-muted transition-colors group-hover:text-accent" />
                {b.public_read ? <Badge tone="ok">public</Badge> : <Badge>private</Badge>}
              </div>
              <div className="truncate font-medium">{b.name}</div>
              <div className="text-xs text-muted">
                {b.objects} object{b.objects === 1 ? "" : "s"}
              </div>
            </Link>
          ))}
        </div>
      )}
    </div>
  );
}

function Stat({
  icon,
  label,
  value,
  sub,
  accent,
}: {
  icon: ReactNode;
  label: string;
  value: string;
  sub?: string;
  accent?: boolean;
}) {
  return (
    <div className="rounded-xl border border-border bg-panel p-4">
      <div className="mb-2 flex items-center gap-2 text-muted">
        {icon}
        <span className="text-xs">{label}</span>
      </div>
      <div className={"text-2xl font-semibold tracking-tight " + (accent ? "text-accent" : "")}>
        {value}
      </div>
      {sub && <div className="mt-0.5 text-xs text-faint">{sub}</div>}
    </div>
  );
}
