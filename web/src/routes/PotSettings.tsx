import { useEffect, useState, type ReactNode } from "react";
import { Link, useParams } from "react-router-dom";
import { useQuery } from "@tanstack/react-query";
import { api, type PotConfig } from "@/lib/api";
import { useToast } from "@/lib/toast";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Select } from "@/components/ui/select";
import { Switch } from "@/components/ui/switch";

export function PotSettings() {
  const { bucket = "" } = useParams();
  const toast = useToast();
  const cfgQuery = useQuery({
    queryKey: ["config", bucket],
    queryFn: () => api.getPotConfig(bucket),
  });
  const [cfg, setCfg] = useState<PotConfig | null>(null);
  const [saving, setSaving] = useState(false);

  useEffect(() => {
    if (cfgQuery.data) setCfg(cfgQuery.data);
  }, [cfgQuery.data]);

  if (!cfg) {
    return <div className="p-8 text-sm text-muted">Loading…</div>;
  }

  const set = (patch: Partial<PotConfig>) => setCfg({ ...cfg, ...patch });

  async function save() {
    setSaving(true);
    try {
      await api.setPotConfig(bucket, cfg!);
      toast("Settings saved", "success");
    } catch {
      toast("Save failed", "error");
    } finally {
      setSaving(false);
    }
  }

  return (
    <div className="mx-auto h-full max-w-2xl overflow-y-auto p-8">
      <p className="mb-1 text-sm">
        <Link to={`/p/${encodeURIComponent(bucket)}`} className="text-accent">
          ← {bucket}
        </Link>
      </p>
      <h1 className="mb-6 text-lg font-semibold tracking-tight">Pot settings</h1>

      <Section title="Visibility">
        <RowSwitch
          label="Public read"
          hint="Anyone can read this pot's objects without a key."
          checked={cfg.public_read}
          onChange={(v) => set({ public_read: v })}
        />
      </Section>

      <Section title="Storage policy" hint="Applies to new uploads. Leave codec on Default to follow the server.">
        <RowField label="Codec">
          <Select
            value={cfg.codec ?? ""}
            onChange={(e) => set({ codec: e.target.value || null })}
            className="max-w-40"
          >
            <option value="">Default</option>
            <option value="zstd">zstd</option>
            <option value="none">none</option>
          </Select>
        </RowField>
        <RowField label="zstd level">
          <Input
            type="number"
            value={cfg.zstd_level ?? ""}
            onChange={(e) =>
              set({ zstd_level: e.target.value === "" ? null : Number(e.target.value) })
            }
            className="max-w-40"
            placeholder="default"
          />
        </RowField>
        <RowField label="Fidelity">
          <Select
            value={cfg.fidelity ?? ""}
            onChange={(e) => set({ fidelity: e.target.value || null })}
            className="max-w-40"
          >
            <option value="">Exact (default)</option>
            <option value="exact">Exact</option>
            <option value="perceptual">Perceptual</option>
          </Select>
        </RowField>
        <RowSwitch
          label="Route images separately"
          hint="Record image uploads on the image route in the manifest."
          checked={cfg.route_by_content_type}
          onChange={(v) => set({ route_by_content_type: v })}
        />
      </Section>

      <Section title="Lifecycle" hint="Enforced periodically in the background.">
        <RowField label="Keep versions">
          <Input
            type="number"
            value={cfg.max_versions ?? ""}
            onChange={(e) =>
              set({ max_versions: e.target.value === "" ? null : Number(e.target.value) })
            }
            className="max-w-40"
            placeholder="unlimited"
          />
        </RowField>
        <RowField label="Expire after (days)">
          <Input
            type="number"
            value={cfg.expire_after_days ?? ""}
            onChange={(e) =>
              set({ expire_after_days: e.target.value === "" ? null : Number(e.target.value) })
            }
            className="max-w-40"
            placeholder="never"
          />
        </RowField>
      </Section>

      <Button onClick={save} disabled={saving}>
        {saving ? "Saving…" : "Save settings"}
      </Button>
    </div>
  );
}

function Section({
  title,
  hint,
  children,
}: {
  title: string;
  hint?: string;
  children: ReactNode;
}) {
  return (
    <section className="mb-6 rounded-xl border border-border bg-panel p-5">
      <h2 className="text-sm font-semibold">{title}</h2>
      {hint && <p className="mb-3 mt-0.5 text-xs text-faint">{hint}</p>}
      <div className="mt-3 space-y-3">{children}</div>
    </section>
  );
}

function RowField({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <div className="flex items-center justify-between gap-4">
      <span className="text-sm text-muted">{label}</span>
      {children}
    </div>
  );
}

function RowSwitch({
  label,
  hint,
  checked,
  onChange,
}: {
  label: string;
  hint?: string;
  checked: boolean;
  onChange: (v: boolean) => void;
}) {
  return (
    <div className="flex items-center justify-between gap-4">
      <div>
        <div className="text-sm">{label}</div>
        {hint && <div className="text-xs text-faint">{hint}</div>}
      </div>
      <Switch checked={checked} onChange={onChange} />
    </div>
  );
}
