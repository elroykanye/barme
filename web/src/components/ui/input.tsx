import type { InputHTMLAttributes } from "react";
import { cn } from "@/lib/cn";

export function Input({ className, ...props }: InputHTMLAttributes<HTMLInputElement>) {
  return (
    <input
      className={cn(
        "w-full rounded-md border border-border bg-panel px-3 py-2 text-sm text-text outline-none transition-colors placeholder:text-faint focus:border-accent",
        className,
      )}
      {...props}
    />
  );
}
