import { useState, type FormEvent } from "react";
import { useAuth } from "@/lib/auth";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { ApiError } from "@/lib/api";

export function Login() {
  const { login } = useAuth();
  const [access, setAccess] = useState("");
  const [secret, setSecret] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  async function onSubmit(e: FormEvent) {
    e.preventDefault();
    setBusy(true);
    setError(null);
    try {
      await login(access, secret);
    } catch (err) {
      setError(
        err instanceof ApiError && err.status === 403
          ? "Invalid credentials."
          : "Could not reach the server.",
      );
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className="flex min-h-full items-center justify-center p-6">
      <form
        onSubmit={onSubmit}
        className="w-full max-w-sm rounded-xl border border-border bg-panel p-7 shadow-2xl shadow-black/40"
      >
        <div className="mb-6 flex items-center gap-2.5">
          <span className="size-2.5 rounded-full bg-gradient-to-br from-accent to-fuchsia-400 shadow-[0_0_0_3px] shadow-accent/20" />
          <span className="text-base font-semibold tracking-tight">barme</span>
        </div>

        <h1 className="mb-1 text-sm font-medium">Sign in</h1>
        <p className="mb-5 text-xs text-muted">
          Enter the owner credentials this instance was started with.
        </p>

        <label className="mb-3 block">
          <span className="mb-1.5 block text-xs text-muted">Access key</span>
          <Input value={access} onChange={(e) => setAccess(e.target.value)} autoFocus />
        </label>
        <label className="mb-4 block">
          <span className="mb-1.5 block text-xs text-muted">Secret key</span>
          <Input
            type="password"
            value={secret}
            onChange={(e) => setSecret(e.target.value)}
          />
        </label>

        {error && <p className="mb-3 text-xs text-danger">{error}</p>}

        <Button type="submit" className="w-full" disabled={busy}>
          {busy ? "Signing in…" : "Sign in"}
        </Button>
      </form>
    </div>
  );
}
