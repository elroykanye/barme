import { useState, type ReactNode } from "react";
import { cn } from "@/lib/cn";

/** Lightweight hover tooltip. Wraps its child in an inline-flex anchor. */
export function Tooltip({
  label,
  children,
  className,
}: {
  label: ReactNode;
  children: ReactNode;
  className?: string;
}) {
  const [show, setShow] = useState(false);
  return (
    <span
      className={cn("relative inline-flex", className)}
      onMouseEnter={() => setShow(true)}
      onMouseLeave={() => setShow(false)}
    >
      {children}
      {show && (
        <span className="pointer-events-none absolute bottom-full left-1/2 z-50 mb-1.5 -translate-x-1/2 whitespace-nowrap rounded-md border border-border bg-elevated px-2 py-1 text-[11px] text-text shadow-lg shadow-black/40">
          {label}
        </span>
      )}
    </span>
  );
}
