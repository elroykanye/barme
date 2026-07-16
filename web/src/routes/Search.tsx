import { useEffect, useState } from "react";
import { Link, useSearchParams } from "react-router-dom";
import { Search as SearchIcon } from "lucide-react";
import { api, publicUrl, type SearchHit } from "@/lib/api";
import { shortHash } from "@/lib/format";
import { Input } from "@/components/ui/input";
import { Badge } from "@/components/ui/badge";

const IMG_RE = /\.(png|jpe?g|gif|webp|avif|svg|bmp)$/i;

export function SearchPage() {
  const [params, setParams] = useSearchParams();
  const [q, setQ] = useState(params.get("q") ?? "");
  const [hits, setHits] = useState<SearchHit[]>([]);
  const [busy, setBusy] = useState(false);
  const [ran, setRan] = useState(false);

  useEffect(() => {
    const term = q.trim();
    if (!term) {
      setHits([]);
      setRan(false);
      return;
    }
    const t = setTimeout(async () => {
      setBusy(true);
      setParams({ q: term }, { replace: true });
      try {
        setHits(await api.search(term));
      } catch {
        setHits([]);
      } finally {
        setBusy(false);
        setRan(true);
      }
    }, 300);
    return () => clearTimeout(t);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [q]);

  // Group results by pot for a scannable layout.
  const groups = new Map<string, SearchHit[]>();
  for (const h of hits) {
    const pot = h.pot ?? "unknown";
    (groups.get(pot) ?? groups.set(pot, []).get(pot)!).push(h);
  }

  return (
    <div className="mx-auto h-full max-w-4xl overflow-y-auto p-8">
      <h1 className="mb-5 text-lg font-semibold tracking-tight">Search</h1>

      <div className="relative mb-6">
        <SearchIcon className="pointer-events-none absolute left-3 top-1/2 size-4 -translate-y-1/2 text-faint" />
        <Input
          autoFocus
          value={q}
          onChange={(e) => setQ(e.target.value)}
          placeholder="Search objects by meaning across all pots…"
          className="pl-9"
        />
      </div>

      {busy && <p className="text-sm text-muted">Searching…</p>}

      {!busy && ran && hits.length === 0 && (
        <p className="text-sm text-faint">
          No matches. Semantic search needs BARME_EMBED_URL set on the server.
        </p>
      )}

      <div className="space-y-6">
        {[...groups.entries()].map(([pot, list]) => (
          <div key={pot}>
            <div className="mb-2 flex items-center gap-2">
              <Link to={`/p/${encodeURIComponent(pot)}`} className="text-sm font-medium text-accent">
                {pot}
              </Link>
              <Badge>{list.length}</Badge>
            </div>
            <div className="grid grid-cols-1 gap-2 sm:grid-cols-2">
              {list.map((h) => {
                const to =
                  h.pot && h.key
                    ? `/p/${encodeURIComponent(h.pot)}?key=${encodeURIComponent(h.key)}`
                    : "#";
                return (
                  <Link
                    key={h.id + (h.key ?? "")}
                    to={to}
                    className="flex items-center gap-3 rounded-lg border border-border bg-panel p-2.5 transition-colors hover:border-accent/50"
                  >
                    <div className="flex size-12 shrink-0 items-center justify-center overflow-hidden rounded bg-bg">
                      {h.pot && h.key && IMG_RE.test(h.key) ? (
                        <img
                          src={publicUrl(h.pot, h.key)}
                          alt={h.key}
                          loading="lazy"
                          className="size-full object-cover"
                          onError={(e) => (e.currentTarget.style.visibility = "hidden")}
                        />
                      ) : (
                        <span className="font-mono text-[10px] text-faint">{shortHash(h.id)}</span>
                      )}
                    </div>
                    <div className="min-w-0 flex-1">
                      <div className="truncate text-sm">{h.key ?? shortHash(h.id)}</div>
                      <div className="text-xs text-faint">score {h.score.toFixed(3)}</div>
                    </div>
                  </Link>
                );
              })}
            </div>
          </div>
        ))}
      </div>
    </div>
  );
}
