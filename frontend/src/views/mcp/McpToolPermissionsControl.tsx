import { useRef } from "react";

import type { ApprovalMode } from "@/api/mcp-tool-policy";
import { cn } from "@/lib/utils";

/** What each mode does, in the words the operator is choosing between. */
export const MODE_LABELS: Record<ApprovalMode, string> = {
  always_allow: "Allow",
  needs_approval: "Needs approval",
  blocked: "Block",
};

const MODES = Object.keys(MODE_LABELS) as ApprovalMode[];

const ARROWS = ["ArrowDown", "ArrowRight", "ArrowUp", "ArrowLeft"];

interface Props {
  value: ApprovalMode;
  label: string;
  disabled: boolean;
  onChange: (mode: ApprovalMode) => void;
}

export function ModeChoice({ value, label, disabled, onChange }: Props) {
  const radios = useRef<(HTMLButtonElement | null)[]>([]);

  function handleKeyDown(event: React.KeyboardEvent<HTMLDivElement>) {
    if (disabled || !ARROWS.includes(event.key)) return;
    const step =
      event.key === "ArrowDown" || event.key === "ArrowRight" ? 1 : -1;
    const focused = radios.current.indexOf(event.target as HTMLButtonElement);
    if (focused === -1) return;
    event.preventDefault();
    const next = (focused + step + MODES.length) % MODES.length;
    radios.current[next]?.focus();
    const mode = MODES[next];
    if (mode && mode !== value) onChange(mode);
  }

  return (
    <div
      role="radiogroup"
      aria-label={label}
      className="inline-flex shrink-0 rounded-md border border-border p-0.5"
      onKeyDown={handleKeyDown}
    >
      {MODES.map((mode, index) => {
        const active = mode === value;
        return (
          <button
            key={mode}
            ref={(el) => {
              radios.current[index] = el;
            }}
            type="button"
            role="radio"
            aria-checked={active}
            tabIndex={active ? 0 : -1}
            disabled={disabled}
            data-testid={`mcp-mode-${mode}`}
            onClick={() => {
              if (!active) onChange(mode);
            }}
            className={cn(
              "rounded px-2 py-0.5 text-xs font-medium transition-colors",
              "disabled:cursor-not-allowed disabled:opacity-60",
              active
                ? "bg-secondary text-secondary-foreground"
                : "text-muted-foreground hover:bg-muted",
            )}
          >
            {MODE_LABELS[mode]}
          </button>
        );
      })}
    </div>
  );
}
