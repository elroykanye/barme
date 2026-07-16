import { createContext, useCallback, useContext, useRef, useState, type ReactNode } from "react";
import { Dialog } from "@/components/ui/dialog";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";

interface PromptOpts {
  title: string;
  label?: string;
  defaultValue?: string;
  placeholder?: string;
  submitLabel?: string;
}
interface ConfirmOpts {
  title: string;
  message: string;
  confirmLabel?: string;
  danger?: boolean;
}

type State =
  | { kind: "prompt"; opts: PromptOpts; value: string }
  | { kind: "confirm"; opts: ConfirmOpts }
  | null;

interface DialogsValue {
  prompt: (o: PromptOpts) => Promise<string | null>;
  confirm: (o: ConfirmOpts) => Promise<boolean>;
}

const Ctx = createContext<DialogsValue | null>(null);

/** Imperative replacements for window.prompt / window.confirm, styled and
 *  promise-based, so call sites read `const name = await prompt({...})`. */
export function DialogProvider({ children }: { children: ReactNode }) {
  const [state, setState] = useState<State>(null);
  const resolver = useRef<((v: unknown) => void) | null>(null);

  const prompt = useCallback(
    (opts: PromptOpts) =>
      new Promise<string | null>((res) => {
        resolver.current = res as (v: unknown) => void;
        setState({ kind: "prompt", opts, value: opts.defaultValue ?? "" });
      }),
    [],
  );
  const confirm = useCallback(
    (opts: ConfirmOpts) =>
      new Promise<boolean>((res) => {
        resolver.current = res as (v: unknown) => void;
        setState({ kind: "confirm", opts });
      }),
    [],
  );

  function close(result: string | null | boolean) {
    setState(null);
    resolver.current?.(result);
    resolver.current = null;
  }

  return (
    <Ctx.Provider value={{ prompt, confirm }}>
      {children}

      {state?.kind === "prompt" && (
        <Dialog
          open
          title={state.opts.title}
          onClose={() => close(null)}
          footer={
            <>
              <Button variant="ghost" onClick={() => close(null)}>
                Cancel
              </Button>
              <Button onClick={() => close(state.value)}>{state.opts.submitLabel ?? "Save"}</Button>
            </>
          }
        >
          <form
            onSubmit={(e) => {
              e.preventDefault();
              close(state.value);
            }}
          >
            {state.opts.label && (
              <label className="mb-1.5 block text-xs text-muted">{state.opts.label}</label>
            )}
            <Input
              autoFocus
              value={state.value}
              placeholder={state.opts.placeholder}
              onChange={(e) =>
                setState((s) => (s && s.kind === "prompt" ? { ...s, value: e.target.value } : s))
              }
            />
          </form>
        </Dialog>
      )}

      {state?.kind === "confirm" && (
        <Dialog
          open
          title={state.opts.title}
          onClose={() => close(false)}
          footer={
            <>
              <Button variant="ghost" onClick={() => close(false)}>
                Cancel
              </Button>
              <Button
                variant={state.opts.danger ? "danger" : "primary"}
                onClick={() => close(true)}
              >
                {state.opts.confirmLabel ?? "Confirm"}
              </Button>
            </>
          }
        >
          <p className="text-sm text-muted">{state.opts.message}</p>
        </Dialog>
      )}
    </Ctx.Provider>
  );
}

export function useDialogs() {
  const v = useContext(Ctx);
  if (!v) throw new Error("useDialogs outside DialogProvider");
  return v;
}
