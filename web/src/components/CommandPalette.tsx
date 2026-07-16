import { useEffect, useState } from "react";
import { useNavigate } from "react-router-dom";
import { useQuery } from "@tanstack/react-query";
import { CornerDownLeft, Search } from "lucide-react";
import { api, publicUrl, type SearchHit } from "@/lib/api";
import { shortHash } from "@/lib/format";

const IMG_RE = /\.(png|jpe?g|gif|webp|avif|svg|bmp)$/i;

export function CommandPalette({ onClose }: { onClose: () => void }) {
  const navigate = useNavigate();
  const [q, setQ] = useState("");
  const [hits, setHits] = useState<SearchHit[]>([]);
  const [searching, setSearching] = useState(false);
  const buckets = useQuery({ queryKey: ["buckets"], queryFn: api.listBuckets });

  useEffect(() => {
    if (!q.trim()) {
      setHits([]);
      return;
    }
    const t = setTimeout(async () => {
      setSearching(true);
      try {
        setHits(await api.search(q.trim()));
      } catch {
        setHits([]);
      } finally {
        setSearching(false);
      }
    }, 250);
    return () => clearTimeout(t);
  }, [q]);

  const bucketMatches = (buckets.data ?? []).filter((b) =>
    b.name.toLowerCase().includes(q.toLowerCase()),
  );

  function openHit(h: SearchHit) {
    if (h.pot && h.key) {
      navigate(`/p/${encodeURIComponent(h.pot)}?key=${encodeURIComponent(h.key)}`);
      onClose();
    }
  }

  function seeAll() {
    navigate(`/search?q=${encodeURIComponent(q.trim())}`);
    onClose();
  }

  return (
    <div
      className="fixed inset-0 z-50 flex items-start justify-center bg-black/50 pt-[15vh]"
      onClick={onClose}
    >
      <div
        className="w-full max-w-lg overflow-hidden rounded-xl border border-border bg-panel shadow-2xl shadow-black/50"
        onClick={(e) => e.stopPropagation()}
      >
        <div className="flex items-center gap-2 border-b border-border px-4">
          <Search className="size-4 text-muted" />
          <input
            autoFocus
            value={q}
            onChange={(e) => setQ(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Escape") onClose();
              if (e.key === "Enter" && q.trim()) seeAll();
            }}
            placeholder="Search pots, or objects by meaning…"
            className="w-full bg-transparent py-3.5 text-sm outline-none placeholder:text-faint"
          />
        </div>

        <div className="max-h-80 overflow-y-auto p-2">
          {bucketMatches.length > 0 && (
            <div className="px-2 py-1 text-[11px] uppercase tracking-wider text-faint">Pots</div>
          )}
          {bucketMatches.map((b) => (
            <button
              key={b.name}
              onClick={() => {
                navigate(`/p/${encodeURIComponent(b.name)}`);
                onClose();
              }}
              className="flex w-full items-center rounded-md px-2 py-2 text-left text-sm hover:bg-elevated"
            >
              {b.name}
            </button>
          ))}

          {q && (
            <div className="px-2 py-1 text-[11px] uppercase tracking-wider text-faint">
              {searching ? "Searching…" : "Semantic results"}
            </div>
          )}
          {hits.map((h) => (
            <button
              key={h.id + (h.key ?? "")}
              onClick={() => openHit(h)}
              className="flex w-full items-center gap-2.5 rounded-md px-2 py-2 text-left text-sm hover:bg-elevated"
            >
              <span className="flex size-8 shrink-0 items-center justify-center overflow-hidden rounded bg-bg">
                {h.pot && h.key && IMG_RE.test(h.key) ? (
                  <img
                    src={publicUrl(h.pot, h.key)}
                    alt=""
                    loading="lazy"
                    className="size-full object-cover"
                    onError={(e) => (e.currentTarget.style.visibility = "hidden")}
                  />
                ) : (
                  <Search className="size-3.5 text-faint" />
                )}
              </span>
              <span className="min-w-0 flex-1">
                <span className="block truncate">{h.key ?? shortHash(h.id)}</span>
                {h.pot && <span className="block text-[11px] text-faint">{h.pot}</span>}
              </span>
              <span className="text-xs text-faint">{h.score.toFixed(3)}</span>
            </button>
          ))}

          {q && !searching && (
            <button
              onClick={seeAll}
              className="mt-1 flex w-full items-center gap-2 rounded-md px-2 py-2 text-left text-xs text-muted hover:bg-elevated hover:text-text"
            >
              <CornerDownLeft className="size-3.5" /> See all results for “{q.trim()}”
            </button>
          )}

          {q && !searching && hits.length === 0 && bucketMatches.length === 0 && (
            <p className="px-2 py-3 text-xs text-faint">
              No matches. Semantic search needs BARME_EMBED_URL set on the server.
            </p>
          )}
        </div>
      </div>
    </div>
  );
}
