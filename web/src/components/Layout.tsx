import { useEffect, useState } from "react";
import { Link, Outlet, useNavigate, useParams } from "react-router-dom";
import { useQuery } from "@tanstack/react-query";
import { Database, LogOut, Plus, Search } from "lucide-react";
import { api } from "@/lib/api";
import { useAuth } from "@/lib/auth";
import { cn } from "@/lib/cn";
import { CommandPalette } from "./CommandPalette";

export function Layout() {
  const { bucket } = useParams();
  const navigate = useNavigate();
  const { logout, creds } = useAuth();
  const [paletteOpen, setPaletteOpen] = useState(false);
  const buckets = useQuery({ queryKey: ["buckets"], queryFn: api.listBuckets });

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === "k") {
        e.preventDefault();
        setPaletteOpen((v) => !v);
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);

  function newBucket() {
    const name = window.prompt("New bucket name")?.trim();
    if (name) navigate(`/b/${encodeURIComponent(name)}`);
  }

  return (
    <div className="grid h-full grid-cols-[240px_1fr]">
      <aside className="flex min-h-0 flex-col border-r border-border bg-panel">
        <Link to="/" className="flex h-14 items-center gap-2.5 border-b border-border px-4">
          <span className="size-2.5 rounded-full bg-gradient-to-br from-accent to-fuchsia-400 shadow-[0_0_0_3px] shadow-accent/20" />
          <span className="font-semibold tracking-tight">barme</span>
        </Link>

        <div className="flex items-center justify-between px-4 pb-2 pt-4">
          <span className="text-[11px] uppercase tracking-wider text-faint">Buckets</span>
          <button onClick={newBucket} className="text-muted transition-colors hover:text-text">
            <Plus className="size-4" />
          </button>
        </div>

        <nav className="min-h-0 flex-1 overflow-y-auto px-2 pb-4">
          {buckets.data?.length ? (
            buckets.data.map((b) => (
              <Link
                key={b.name}
                to={`/b/${encodeURIComponent(b.name)}`}
                className={cn(
                  "flex items-center justify-between rounded-md px-2 py-1.5 text-sm transition-colors",
                  bucket === b.name
                    ? "bg-elevated text-text"
                    : "text-muted hover:bg-elevated/60 hover:text-text",
                )}
              >
                <span className="flex min-w-0 items-center gap-2">
                  <Database className="size-3.5 shrink-0" />
                  <span className="truncate">{b.name}</span>
                </span>
                {b.public_read && (
                  <span className="size-1.5 shrink-0 rounded-full bg-ok" title="public" />
                )}
              </Link>
            ))
          ) : (
            <p className="px-2 text-xs text-faint">No buckets yet.</p>
          )}
        </nav>
      </aside>

      <div className="flex min-w-0 flex-col">
        <header className="flex h-14 items-center justify-between gap-4 border-b border-border px-5">
          <button
            onClick={() => setPaletteOpen(true)}
            className="flex items-center gap-2 rounded-md border border-border bg-panel px-3 py-1.5 text-sm text-muted transition-colors hover:text-text"
          >
            <Search className="size-4" />
            Search
            <kbd className="ml-6 rounded bg-elevated px-1.5 py-0.5 font-mono text-[11px]">⌘K</kbd>
          </button>
          <div className="flex items-center gap-3 text-sm">
            <span className="text-muted">{creds?.access}</span>
            <button onClick={logout} className="text-muted transition-colors hover:text-danger" title="Sign out">
              <LogOut className="size-4" />
            </button>
          </div>
        </header>
        <main className="min-h-0 flex-1 overflow-hidden">
          <Outlet />
        </main>
      </div>

      {paletteOpen && <CommandPalette onClose={() => setPaletteOpen(false)} />}
    </div>
  );
}
