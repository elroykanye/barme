import { useState } from "react";
import { Copy, Globe, Link2 } from "lucide-react";
import { api, cdnUrl, publicUrl } from "@/lib/api";
import { copyText } from "@/lib/clipboard";
import { useToast } from "@/lib/toast";
import { Button } from "@/components/ui/button";
import { Select } from "@/components/ui/select";
import { Dialog } from "@/components/ui/dialog";
import { QRCode } from "@/components/ui/qrcode";

const EXPIRIES: { label: string; secs: number }[] = [
  { label: "1 hour", secs: 3600 },
  { label: "1 day", secs: 86400 },
  { label: "7 days", secs: 604800 },
];

export function ShareDialog({
  bucket,
  objectKey,
  objectId,
  isPublic,
  onClose,
}: {
  bucket: string;
  objectKey: string;
  objectId: string;
  isPublic: boolean;
  onClose: () => void;
}) {
  const toast = useToast();
  const [secs, setSecs] = useState(EXPIRIES[1].secs);
  const [url, setUrl] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const shown = url ?? cdnUrl(objectId);

  async function presign() {
    setBusy(true);
    try {
      const r = await api.presign(bucket, objectKey, secs);
      setUrl(r.url);
      toast("Presigned link ready", "success");
    } catch {
      toast("Could not presign", "error");
    } finally {
      setBusy(false);
    }
  }

  function copy(u: string, label: string) {
    copyText(u)
      .then(() => toast(`${label} copied`, "success"))
      .catch(() => toast("Copy failed", "error"));
  }

  return (
    <Dialog
      open
      title="Share object"
      onClose={onClose}
      footer={
        <Button variant="ghost" onClick={onClose}>
          Done
        </Button>
      }
    >
      <div className="flex flex-col items-center gap-4">
        <QRCode value={shown} className="rounded-lg border border-border" />

        <div className="flex w-full items-center gap-2">
          <input
            readOnly
            value={shown}
            className="min-w-0 flex-1 truncate rounded-md border border-border bg-bg px-3 py-2 text-xs text-muted outline-none"
          />
          <Button variant="outline" onClick={() => copy(shown, "Link")} title="Copy link">
            <Copy className="size-4" />
          </Button>
        </div>

        <div className="flex w-full items-center gap-2">
          <Select
            value={secs}
            onChange={(e) => setSecs(Number(e.target.value))}
            className="max-w-32"
          >
            {EXPIRIES.map((e) => (
              <option key={e.secs} value={e.secs}>
                {e.label}
              </option>
            ))}
          </Select>
          <Button className="flex-1" onClick={presign} disabled={busy}>
            <Link2 className="size-4" />
            {busy ? "Signing…" : "Presign link"}
          </Button>
        </div>

        <div className="flex w-full flex-col gap-2 border-t border-border pt-3">
          <button
            onClick={() => copy(cdnUrl(objectId), "Immutable link")}
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
      </div>
    </Dialog>
  );
}
