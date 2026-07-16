import { cn } from "@/lib/cn";

export function Tabs<T extends string>({
  tabs,
  active,
  onChange,
  className,
}: {
  tabs: readonly T[];
  active: T;
  onChange: (t: T) => void;
  className?: string;
}) {
  return (
    <div className={cn("flex gap-1 border-b border-border", className)}>
      {tabs.map((t) => (
        <button
          key={t}
          onClick={() => onChange(t)}
          className={cn(
            "-mb-px border-b-2 px-3 py-2 text-sm transition-colors",
            active === t
              ? "border-accent text-text"
              : "border-transparent text-muted hover:text-text",
          )}
        >
          {t}
        </button>
      ))}
    </div>
  );
}
