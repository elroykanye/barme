import { useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Copy, KeyRound, Plus, Trash2 } from "lucide-react";
import { api, type NewKey, type Webhook } from "@/lib/api";
import { copyText } from "@/lib/clipboard";
import { useToast } from "@/lib/toast";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Switch } from "@/components/ui/switch";
import { Badge } from "@/components/ui/badge";
import { Dialog } from "@/components/ui/dialog";
import { Checkbox } from "@/components/ui/checkbox";
import { Tabs } from "@/components/ui/tabs";
import { StatusView } from "@/routes/Status";

const TABS = ["Access keys", "Webhooks", "Status", "Server"] as const;

export function Settings() {
  const [tab, setTab] = useState<(typeof TABS)[number]>("Access keys");

  return (
    <div className="mx-auto h-full max-w-3xl overflow-y-auto p-8">
      <h1 className="mb-5 text-lg font-semibold tracking-tight">Settings</h1>

      <Tabs tabs={TABS} active={tab} onChange={setTab} className="mb-6" />

      {tab === "Access keys" && <Keys />}
      {tab === "Webhooks" && <Webhooks />}
      {tab === "Status" && <StatusView />}
      {tab === "Server" && <Server />}
    </div>
  );
}

function genSecret(): string {
  const b = new Uint8Array(24);
  crypto.getRandomValues(b);
  return Array.from(b)
    .map((x) => x.toString(16).padStart(2, "0"))
    .join("");
}

function Keys() {
  const qc = useQueryClient();
  const toast = useToast();
  const keys = useQuery({ queryKey: ["keys"], queryFn: api.listKeys });
  const [creating, setCreating] = useState(false);
  const [revealed, setRevealed] = useState<{ access: string; secret: string } | null>(null);

  const create = useMutation({
    mutationFn: (k: NewKey) => api.createKey(k),
    onSuccess: (_v, k) => {
      qc.invalidateQueries({ queryKey: ["keys"] });
      setRevealed({ access: k.access_key, secret: k.secret_key });
    },
    onError: () => toast("Could not create key", "error"),
  });
  const remove = useMutation({
    mutationFn: (access: string) => api.deleteKey(access),
    onSuccess: () => qc.invalidateQueries({ queryKey: ["keys"] }),
  });

  return (
    <div>
      <div className="mb-3 flex items-center justify-between">
        <p className="text-sm text-muted">Credentials for the S3 and native APIs.</p>
        <Button onClick={() => setCreating(true)}>
          <Plus className="size-4" />
          New key
        </Button>
      </div>

      <div className="overflow-hidden rounded-xl border border-border">
        {!keys.data?.length ? (
          <div className="p-8 text-center text-sm text-muted">
            No keys. The server runs open until you add one.
          </div>
        ) : (
          <table className="w-full text-sm">
            <thead>
              <tr className="border-b border-border text-left text-[11px] uppercase tracking-wider text-faint">
                <th className="px-4 py-2.5 font-medium">Access key</th>
                <th className="px-4 py-2.5 font-medium">Scope</th>
                <th className="px-4 py-2.5 font-medium">Access</th>
                <th />
              </tr>
            </thead>
            <tbody>
              {keys.data.map((k) => (
                <tr key={k.access_key} className="border-b border-border/60 last:border-0">
                  <td className="px-4 py-2.5 font-mono text-xs">{k.access_key}</td>
                  <td className="px-4 py-2.5 text-muted">
                    {k.pots.length ? k.pots.join(", ") : "all pots"}
                  </td>
                  <td className="px-4 py-2.5">
                    {k.read_only ? <Badge tone="warn">read-only</Badge> : <Badge tone="ok">full</Badge>}
                  </td>
                  <td className="px-4 py-2.5 text-right">
                    <button
                      onClick={() => remove.mutate(k.access_key)}
                      className="text-muted transition-colors hover:text-danger"
                      title="Delete key"
                    >
                      <Trash2 className="size-4" />
                    </button>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </div>

      {creating && (
        <CreateKey
          onClose={() => setCreating(false)}
          onCreate={(k) => {
            setCreating(false);
            create.mutate(k);
          }}
        />
      )}

      {revealed && (
        <Dialog
          open
          title="Key created"
          onClose={() => setRevealed(null)}
          footer={<Button onClick={() => setRevealed(null)}>Done</Button>}
        >
          <p className="mb-3 text-xs text-muted">
            Copy the secret now, it isn't shown again.
          </p>
          <Field label="Access key" value={revealed.access} onCopy={copyText} toast={toast} />
          <Field label="Secret key" value={revealed.secret} onCopy={copyText} toast={toast} />
        </Dialog>
      )}
    </div>
  );
}

function Field({
  label,
  value,
  onCopy,
  toast,
}: {
  label: string;
  value: string;
  onCopy: (t: string) => Promise<void>;
  toast: (m: string, k?: "info" | "success" | "error") => void;
}) {
  return (
    <div className="mb-2">
      <div className="mb-1 text-xs text-faint">{label}</div>
      <div className="flex items-center gap-2 rounded-md border border-border bg-bg px-3 py-2">
        <code className="flex-1 truncate text-xs">{value}</code>
        <button
          onClick={() => onCopy(value).then(() => toast("Copied", "success"))}
          className="text-muted hover:text-text"
        >
          <Copy className="size-3.5" />
        </button>
      </div>
    </div>
  );
}

function CreateKey({
  onClose,
  onCreate,
}: {
  onClose: () => void;
  onCreate: (k: NewKey) => void;
}) {
  const [access, setAccess] = useState("");
  const [readOnly, setReadOnly] = useState(false);
  const [pots, setPots] = useState("");

  function submit() {
    if (!access.trim()) return;
    onCreate({
      access_key: access.trim(),
      secret_key: genSecret(),
      read_only: readOnly,
      pots: pots
        .split(",")
        .map((p) => p.trim())
        .filter(Boolean),
    });
  }

  return (
    <Dialog
      open
      title="New access key"
      onClose={onClose}
      footer={
        <>
          <Button variant="ghost" onClick={onClose}>
            Cancel
          </Button>
          <Button onClick={submit}>Create</Button>
        </>
      }
    >
      <label className="mb-3 block">
        <span className="mb-1.5 block text-xs text-muted">Access key</span>
        <Input value={access} onChange={(e) => setAccess(e.target.value)} placeholder="e.g. ci-uploader" autoFocus />
      </label>
      <label className="mb-3 block">
        <span className="mb-1.5 block text-xs text-muted">Scope (comma-separated pots, blank = all)</span>
        <Input value={pots} onChange={(e) => setPots(e.target.value)} placeholder="photos, backups" />
      </label>
      <div className="flex items-center justify-between">
        <span className="flex items-center gap-2 text-sm">
          <KeyRound className="size-4 text-muted" /> Read-only
        </span>
        <Switch checked={readOnly} onChange={setReadOnly} />
      </div>
    </Dialog>
  );
}

const WEBHOOK_EVENTS = ["object.created", "object.deleted"] as const;

function Webhooks() {
  const qc = useQueryClient();
  const toast = useToast();
  const hooks = useQuery({ queryKey: ["webhooks"], queryFn: api.listWebhooks });
  const [url, setUrl] = useState("");
  const [events, setEvents] = useState<string[]>([...WEBHOOK_EVENTS]);

  const create = useMutation({
    mutationFn: () => api.saveWebhook(url.trim(), events),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ["webhooks"] });
      setUrl("");
      toast("Webhook added", "success");
    },
    onError: () => toast("Could not add webhook", "error"),
  });
  const remove = useMutation({
    mutationFn: (id: string) => api.deleteWebhook(id),
    onSuccess: () => qc.invalidateQueries({ queryKey: ["webhooks"] }),
  });

  function toggleEvent(e: string) {
    setEvents((cur) => (cur.includes(e) ? cur.filter((x) => x !== e) : [...cur, e]));
  }

  return (
    <div>
      <p className="mb-3 text-sm text-muted">
        POST notifications to your endpoints when objects change.
      </p>

      <div className="mb-4 rounded-xl border border-border bg-panel p-4">
        <label className="mb-3 block">
          <span className="mb-1.5 block text-xs text-muted">Endpoint URL</span>
          <Input
            value={url}
            onChange={(e) => setUrl(e.target.value)}
            placeholder="https://example.com/hook"
          />
        </label>
        <div className="mb-3 flex flex-wrap gap-4">
          {WEBHOOK_EVENTS.map((e) => (
            <label key={e} className="flex items-center gap-2 text-sm text-muted">
              <Checkbox checked={events.includes(e)} onChange={() => toggleEvent(e)} />
              {e}
            </label>
          ))}
        </div>
        <Button onClick={() => create.mutate()} disabled={!url.trim() || !events.length || create.isPending}>
          <Plus className="size-4" /> Add webhook
        </Button>
      </div>

      <div className="overflow-hidden rounded-xl border border-border">
        {!hooks.data?.length ? (
          <div className="p-8 text-center text-sm text-muted">No webhooks yet.</div>
        ) : (
          <table className="w-full text-sm">
            <thead>
              <tr className="border-b border-border text-left text-[11px] uppercase tracking-wider text-faint">
                <th className="px-4 py-2.5 font-medium">Endpoint</th>
                <th className="px-4 py-2.5 font-medium">Events</th>
                <th />
              </tr>
            </thead>
            <tbody>
              {hooks.data.map((h: Webhook) => (
                <tr key={h.id} className="border-b border-border/60 last:border-0">
                  <td className="px-4 py-2.5 font-mono text-xs">{h.url}</td>
                  <td className="px-4 py-2.5">
                    <span className="flex flex-wrap gap-1">
                      {h.events.map((e) => (
                        <Badge key={e}>{e}</Badge>
                      ))}
                    </span>
                  </td>
                  <td className="px-4 py-2.5 text-right">
                    <button
                      onClick={() => remove.mutate(h.id)}
                      className="text-muted transition-colors hover:text-danger"
                      title="Delete webhook"
                    >
                      <Trash2 className="size-4" />
                    </button>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </div>
    </div>
  );
}

function Server() {
  const stats = useQuery({ queryKey: ["stats"], queryFn: api.stats });
  return (
    <dl className="space-y-3 rounded-xl border border-border bg-panel p-5 text-sm">
      <Row k="Native API" v={api.base} />
      <Row k="Objects" v={stats.data ? String(stats.data.objects) : "—"} />
      <Row k="Pots" v={stats.data ? String(stats.data.buckets) : "—"} />
      <Row k="Unique chunks" v={stats.data ? String(stats.data.unique_chunks) : "—"} />
    </dl>
  );
}

function Row({ k, v }: { k: string; v: string }) {
  return (
    <div className="flex items-center justify-between">
      <dt className="text-muted">{k}</dt>
      <dd className="font-mono text-xs">{v}</dd>
    </div>
  );
}
