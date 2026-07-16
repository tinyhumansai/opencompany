import { cn } from "@/lib/utils";
import { lifecycle } from "@/lib/language";

const TONE_STYLES: Record<string, { dot: string; text: string }> = {
  live: { dot: "bg-emerald-500", text: "text-emerald-600 dark:text-emerald-400" },
  idle: { dot: "bg-amber-500", text: "text-amber-600 dark:text-amber-400" },
  stopped: { dot: "bg-rose-500", text: "text-rose-600 dark:text-rose-400" },
};

/** A small lifecycle indicator: a colored dot + plain-language label. */
export function StatusPill({
  lifecycle: state,
  className,
}: {
  lifecycle: string;
  className?: string;
}) {
  const { label, tone } = lifecycle(state);
  const style = TONE_STYLES[tone];
  return (
    <span
      className={cn(
        "inline-flex items-center gap-1.5 rounded-full border bg-card px-2.5 py-0.5 text-xs font-medium",
        style.text,
        className,
      )}
    >
      <span className={cn("size-1.5 rounded-full", style.dot, tone === "live" && "animate-pulse")} />
      {label}
    </span>
  );
}
