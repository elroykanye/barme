import type { SelectHTMLAttributes } from "react";
import { cn } from "@/lib/cn";

export function Select({ className, children, ...props }: SelectHTMLAttributes<HTMLSelectElement>) {
  return (
    <select
      className={cn(
        "w-full rounded-md border border-border bg-panel px-3 py-2 text-sm text-text outline-none transition-colors focus:border-accent",
        className,
      )}
      {...props}
    >
      {children}
    </select>
  );
}
