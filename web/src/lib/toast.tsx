import { createContext, useCallback, useContext, useState, type ReactNode } from "react";

type Kind = "info" | "success" | "error";
interface Toast {
  id: number;
  kind: Kind;
  message: string;
}

const ToastContext = createContext<((message: string, kind?: Kind) => void) | null>(null);

let nextId = 1;

export function ToastProvider({ children }: { children: ReactNode }) {
  const [toasts, setToasts] = useState<Toast[]>([]);

  const push = useCallback((message: string, kind: Kind = "info") => {
    const id = nextId++;
    setToasts((t) => [...t, { id, kind, message }]);
    setTimeout(() => setToasts((t) => t.filter((x) => x.id !== id)), 3200);
  }, []);

  return (
    <ToastContext.Provider value={push}>
      {children}
      <div className="pointer-events-none fixed bottom-5 right-5 z-[60] flex flex-col gap-2">
        {toasts.map((t) => (
          <div
            key={t.id}
            className={
              "pointer-events-auto rounded-lg border px-4 py-2.5 text-sm shadow-xl shadow-black/40 " +
              (t.kind === "error"
                ? "border-danger/40 bg-panel text-danger"
                : t.kind === "success"
                  ? "border-ok/40 bg-panel text-ok"
                  : "border-border bg-panel text-text")
            }
          >
            {t.message}
          </div>
        ))}
      </div>
    </ToastContext.Provider>
  );
}

export function useToast() {
  const v = useContext(ToastContext);
  if (!v) throw new Error("useToast outside ToastProvider");
  return v;
}
