import { CheckCircle2, X, XCircle } from "lucide-react";
import type { UploadItem } from "@/lib/uploads";
import { humanSize } from "@/lib/format";
import { Progress } from "@/components/ui/progress";

export function UploadPanel({
  items,
  onClear,
}: {
  items: UploadItem[];
  onClear: () => void;
}) {
  if (!items.length) return null;
  const active = items.filter((i) => i.status === "uploading").length;

  return (
    <div className="fixed bottom-5 left-1/2 z-40 w-80 -translate-x-1/2 overflow-hidden rounded-xl border border-border bg-panel shadow-2xl shadow-black/50 md:left-auto md:right-5 md:translate-x-0">
      <div className="flex items-center justify-between border-b border-border px-3 py-2">
        <span className="text-xs font-medium">
          {active ? `Uploading ${active}…` : "Uploads"}
        </span>
        <button onClick={onClear} className="text-muted hover:text-text" title="Clear finished">
          <X className="size-3.5" />
        </button>
      </div>
      <div className="max-h-64 space-y-2 overflow-y-auto p-3">
        {items.map((it) => {
          const pct = it.total ? it.loaded / it.total : 0;
          return (
            <div key={it.id} className="text-xs">
              <div className="mb-1 flex items-center justify-between gap-2">
                <span className="min-w-0 flex-1 truncate text-muted" title={it.key}>
                  {it.key}
                </span>
                {it.status === "done" ? (
                  <CheckCircle2 className="size-3.5 text-ok" />
                ) : it.status === "error" ? (
                  <XCircle className="size-3.5 text-danger" />
                ) : it.status === "canceled" ? (
                  <span className="text-faint">canceled</span>
                ) : (
                  <button onClick={it.cancel} className="text-faint hover:text-danger" title="Cancel">
                    <X className="size-3.5" />
                  </button>
                )}
              </div>
              <Progress
                value={it.status === "done" ? 1 : pct}
                tone={it.status === "error" ? "danger" : it.status === "done" ? "ok" : "accent"}
              />
              <div className="mt-1 flex justify-between text-[10px] text-faint">
                <span>
                  {humanSize(it.loaded)} / {humanSize(it.total)}
                </span>
                {it.status === "uploading" && it.speed > 0 && (
                  <span>{humanSize(it.speed)}/s</span>
                )}
              </div>
            </div>
          );
        })}
      </div>
    </div>
  );
}
