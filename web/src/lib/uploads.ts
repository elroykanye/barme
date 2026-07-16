import { useCallback, useRef, useState } from "react";
import { uploadWithProgress } from "@/lib/api";

export interface UploadItem {
  id: string;
  key: string;
  loaded: number;
  total: number;
  speed: number; // bytes/sec
  status: "uploading" | "done" | "error" | "canceled";
  cancel: () => void;
}

let uid = 1;

/** Tracks in-flight XHR uploads with per-file progress, speed and cancel. */
export function useUploadManager(onEach?: () => void) {
  const [items, setItems] = useState<UploadItem[]>([]);
  const sampleRef = useRef<Record<string, { t: number; loaded: number }>>({});

  const patch = useCallback((id: string, p: Partial<UploadItem>) => {
    setItems((list) => list.map((it) => (it.id === id ? { ...it, ...p } : it)));
  }, []);

  const start = useCallback(
    (bucket: string, files: { file: File; key: string }[]) => {
      for (const { file, key } of files) {
        const id = String(uid++);
        const handle = uploadWithProgress(bucket, key, file, (loaded, total) => {
          const now = performance.now();
          const prev = sampleRef.current[id] ?? { t: now, loaded: 0 };
          const dt = (now - prev.t) / 1000;
          const speed = dt > 0.2 ? (loaded - prev.loaded) / dt : undefined;
          if (dt > 0.2) sampleRef.current[id] = { t: now, loaded };
          patch(id, { loaded, total, ...(speed !== undefined ? { speed } : {}) });
        });
        setItems((list) => [
          ...list,
          {
            id,
            key,
            loaded: 0,
            total: file.size,
            speed: 0,
            status: "uploading",
            cancel: () => handle.cancel(),
          },
        ]);
        sampleRef.current[id] = { t: performance.now(), loaded: 0 };
        handle.promise
          .then(() => {
            patch(id, { status: "done", loaded: file.size });
            onEach?.();
          })
          .catch((e) => {
            patch(id, { status: e?.message === "aborted" ? "canceled" : "error" });
          });
      }
    },
    [onEach, patch],
  );

  const clearFinished = useCallback(() => {
    setItems((list) => list.filter((it) => it.status === "uploading"));
  }, []);

  const active = items.filter((i) => i.status === "uploading").length;

  return { items, start, clearFinished, active };
}
