import type { ReactNode } from "react";
import { useQuery } from "@tanstack/react-query";
import { Activity, Clock, Database, HardDrive, Layers, Package } from "lucide-react";
import { api } from "@/lib/api";
import { humanSize } from "@/lib/format";

function uptime(secs: number): string {
  const d = Math.floor(secs / 86400);
  const h = Math.floor((secs % 86400) / 3600);
  const m = Math.floor((secs % 3600) / 60);
  const s = Math.floor(secs % 60);
  if (d) return `${d}d ${h}h ${m}m`;
  if (h) return `${h}h ${m}m`;
  if (m) return `${m}m ${s}s`;
  return `${s}s`;
}

/** Health + storage stats. Rendered standalone (/status) and as a Settings tab. */
export function StatusView() {
  const health = useQuery({
    queryKey: ["health"],
    queryFn: api.health,
    refetchInterval: 5000,
  });
  const stats = useQuery({ queryKey: ["stats"], queryFn: api.stats });

  const h = health.data;
  const s = stats.data;
  const saved =
    s && s.logical_bytes > 0 ? Math.max(0, 100 - (100 * s.physical_bytes) / s.logical_bytes) : 0;

  return (
    <div className="space-y-6">
      <div className="flex items-center gap-2 text-sm">
        <span
          className={`size-2 rounded-full ${health.isError ? "bg-danger" : "bg-ok"}`}
          title={health.isError ? "unreachable" : "healthy"}
        />
        <span className="text-muted">
          {health.isError ? "Server unreachable" : "Server healthy"} · {api.base}
        </span>
      </div>

      <div className="grid grid-cols-2 gap-3 lg:grid-cols-4">
        <Stat icon={<Package className="size-4" />} label="Objects" value={h ? String(h.objects) : "—"} />
        <Stat icon={<Database className="size-4" />} label="Pots" value={h ? String(h.pots) : "—"} />
        <Stat icon={<Layers className="size-4" />} label="Unique chunks" value={h ? String(h.unique_chunks) : "—"} />
        <Stat icon={<Clock className="size-4" />} label="Uptime" value={h ? uptime(h.uptime_secs) : "—"} />
      </div>

      <div>
        <h2 className="mb-3 flex items-center gap-2 text-sm font-medium text-muted">
          <HardDrive className="size-4" /> Storage
        </h2>
        <div className="grid grid-cols-2 gap-3 lg:grid-cols-4">
          <Stat
            icon={<HardDrive className="size-4" />}
            label="On disk"
            value={s ? humanSize(s.physical_bytes) : "—"}
          />
          <Stat
            icon={<HardDrive className="size-4" />}
            label="Logical"
            value={s ? humanSize(s.logical_bytes) : "—"}
          />
          <Stat
            icon={<Activity className="size-4" />}
            label="Saved"
            value={s ? `${saved.toFixed(0)}%` : "—"}
            accent
          />
          <Stat
            icon={<Database className="size-4" />}
            label="Buckets"
            value={s ? String(s.buckets) : "—"}
          />
        </div>
      </div>
    </div>
  );
}

export function Status() {
  return (
    <div className="mx-auto h-full max-w-4xl overflow-y-auto p-8">
      <h1 className="mb-5 text-lg font-semibold tracking-tight">Status</h1>
      <StatusView />
    </div>
  );
}

function Stat({
  icon,
  label,
  value,
  accent,
}: {
  icon: ReactNode;
  label: string;
  value: string;
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
    </div>
  );
}
