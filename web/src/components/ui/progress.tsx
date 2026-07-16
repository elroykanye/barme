import { cn } from "@/lib/cn";

export function Progress({
  value,
  className,
  tone = "accent",
}: {
  /** 0..1 */
  value: number;
  className?: string;
  tone?: "accent" | "ok" | "danger";
}) {
  const pct = Math.max(0, Math.min(1, value)) * 100;
  const bar =
    tone === "ok" ? "bg-ok" : tone === "danger" ? "bg-danger" : "bg-accent";
  return (
    <div className={cn("h-1.5 w-full overflow-hidden rounded-full bg-elevated", className)}>
      <div
        className={cn("h-full rounded-full transition-[width] duration-150", bar)}
        style={{ width: `${pct}%` }}
      />
    </div>
  );
}
