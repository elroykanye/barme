import { useQuery } from "@tanstack/react-query";
import { Link } from "react-router-dom";
import { Database } from "lucide-react";
import { api } from "@/lib/api";
import { Badge } from "@/components/ui/badge";

export function Home() {
  const { data, isLoading } = useQuery({ queryKey: ["buckets"], queryFn: api.listBuckets });

  return (
    <div className="mx-auto h-full max-w-4xl overflow-y-auto p-8">
      <h1 className="mb-6 text-lg font-semibold tracking-tight">Buckets</h1>

      {isLoading ? (
        <p className="text-sm text-muted">Loading…</p>
      ) : !data?.length ? (
        <div className="rounded-xl border border-dashed border-border p-12 text-center">
          <p className="text-sm text-muted">No buckets yet.</p>
          <p className="mt-1 text-xs text-faint">
            Create one with the + in the sidebar, then upload a file.
          </p>
        </div>
      ) : (
        <div className="grid grid-cols-1 gap-3 sm:grid-cols-2 lg:grid-cols-3">
          {data.map((b) => (
            <Link
              key={b.name}
              to={`/b/${encodeURIComponent(b.name)}`}
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
