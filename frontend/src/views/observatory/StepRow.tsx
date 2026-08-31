/**
 * One step of an attempt, with its unredacted half behind a fold.
 *
 * The two halves are deliberately labelled differently. `detail`/`result` are
 * the **redacted** projection — the same one an approval card and the chat
 * timeline render — and are always safe. `deep` is the raw arguments and the raw
 * output. A reader should never have to guess which one they are looking at, so
 * the panes say so.
 */

import { useEffect, useRef, useState } from "react";

import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { cn } from "@/lib/utils";
import { formatDuration } from "@/views/workflows/run-health";
import type { ObservatoryStep } from "@/api/observatory";
import { clampText, formatBytes, present } from "./clamp";
import { stepState } from "./model";

/**
 * Tone per step state, matching the waterfall's vocabulary.
 *
 * `idle` is unreachable here — `stepState` never returns it, only `runState`
 * does (issue #1809's decline is a run-level outcome, not a step one) — but
 * `SpanState` is one shared type, so the map must stay exhaustive.
 */
const TONE = {
  done: "text-muted-foreground",
  failed: "text-[var(--status-failed-text)]",
  blocked: "text-[var(--status-blocked-text)]",
  running: "text-[var(--status-running-text)]",
  idle: "text-[var(--status-idle-text)]",
} as const;

const GLYPH: Record<string, string> = {
  thinking: "◇",
  tool_call: "▸",
  note: "·",
};

/** A body pane: a heading, the text, and a fold when it is long. */
function Pane({
  title,
  body,
  mono = true,
}: {
  title: string;
  body: string;
  mono?: boolean;
}) {
  const [expanded, setExpanded] = useState(false);
  const clamped = clampText(body);
  const shown = expanded ? body : clamped.shown;
  return (
    <div className="flex flex-col gap-1">
      <div className="text-muted-foreground flex items-center gap-2 text-3xs uppercase tracking-wide">
        <span>{title}</span>
        {clamped.truncated && !expanded && (
          <span className="normal-case tracking-normal">
            {formatBytes(new Blob([body]).size)} total
          </span>
        )}
      </div>
      <pre
        className={cn(
          "bg-muted/50 max-h-[60vh] overflow-auto rounded p-2 text-xs whitespace-pre-wrap",
          mono ? "font-mono" : "",
        )}
      >
        {shown}
        {clamped.truncated && !expanded ? "…" : ""}
      </pre>
      {clamped.truncated && (
        <Button
          variant="ghost"
          size="sm"
          className="h-6 self-start px-2 text-xs"
          onClick={() => setExpanded((v) => !v)}
        >
          {expanded ? "Show less" : `Show all (${clamped.hidden} more characters)`}
        </Button>
      )}
    </div>
  );
}

export function StepRow({
  step,
  focus,
}: {
  step: ObservatoryStep;
  /** Whether a deep link names this step; scrolls to and opens it. */
  focus?: boolean;
}) {
  const [open, setOpen] = useState(() => focus);
  const rowRef = useRef<HTMLLIElement>(null);
  const state = stepState(step);
  const deep = step.deep;
  const hasBody =
    present(step.detail) ||
    present(step.result) ||
    present(deep?.reasoning) ||
    present(deep?.arguments) ||
    present(deep?.output);

  useEffect(() => {
    if (!focus) return;
    rowRef.current?.scrollIntoView({ block: "nearest" });
    if (hasBody) setOpen(true);
  }, [focus, hasBody]);

  return (
    <li
      ref={rowRef}
      className={cn(
        "border-border/60 border-b last:border-b-0",
        focus && "bg-muted/60",
      )}
    >
      <button
        type="button"
        onClick={() => hasBody && setOpen((v) => !v)}
        disabled={!hasBody}
        className={cn(
          "flex w-full items-baseline gap-2 px-2 py-1.5 text-left text-xs",
          hasBody && "hover:bg-muted/40",
        )}
      >
        <span className={cn("w-3 shrink-0", TONE[state])}>
          {GLYPH[step.kind] ?? "·"}
        </span>
        <span className="text-muted-foreground w-8 shrink-0 tabular-nums">
          {step.seq}
        </span>
        <span className="min-w-0 flex-1 truncate">
          <span className="font-medium">{step.label}</span>
          {present(step.detail) && (
            <span className="text-muted-foreground"> · {step.detail}</span>
          )}
          {present(step.result) && (
            <span className="text-muted-foreground"> → {step.result}</span>
          )}
        </span>
        {step.failure && (
          <Badge variant="outline" className="shrink-0 text-3xs">
            {step.failure}
          </Badge>
        )}
        {/* A reader can tell at a glance which steps have reasoning behind
            them, without opening each one. */}
        {present(deep?.reasoning) && (
          <span className="text-muted-foreground shrink-0 text-3xs">reasoning</span>
        )}
        {step.elapsedMs !== null && (
          <span className="text-muted-foreground shrink-0 tabular-nums">
            {formatDuration(step.elapsedMs)}
          </span>
        )}
      </button>

      {open && hasBody && (
        <div className="flex flex-col gap-3 px-7 pb-3">
          {present(deep?.reasoning) && (
            <Pane title="Reasoning" body={deep!.reasoning!} mono={false} />
          )}
          {present(deep?.arguments) ? (
            <Pane title="Arguments (raw)" body={deep!.arguments!} />
          ) : (
            present(step.detail) && <Pane title="Arguments (redacted)" body={step.detail!} />
          )}
          {present(deep?.output) ? (
            <Pane title="Output (raw)" body={deep!.output!} />
          ) : (
            present(step.result) && <Pane title="Result (summary)" body={step.result!} />
          )}
          {deep?.clipped && (
            <p className="text-muted-foreground text-3xs">
              The store clipped this step to its size cap; the head is shown.
            </p>
          )}
        </div>
      )}
    </li>
  );
}
