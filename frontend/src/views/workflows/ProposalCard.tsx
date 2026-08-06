// Issue #415: what the operator reads before a copilot's change is applied.
//
// The card is the review, so it shows the DIFF and not the proposal: what would
// change about the graph on screen, step by step and connection by connection,
// computed from the candidate graph rather than from the model's op list. A
// change nobody proposed cannot appear in it, and an op that changes nothing
// shows as nothing.
//
// Nothing here mutates anything. Apply is the only path that writes, it goes
// through the same versioned `updateWorkflow` the editor uses, and Dismiss
// writes nothing at all: the card greys out and stays in the transcript so a
// later question can refer to what was turned down.

import { AlertTriangle, ArrowRight, Check, Loader2, Minus, Plus, X } from "lucide-react";

import type { GraphDiff, NodeChange } from "@/api/workflow-proposal";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import { cn } from "@/lib/utils";

/** Where a proposal has got to. Only `pending` can write anything. */
export type ProposalState = "pending" | "applying" | "applied" | "dismissed";

interface Props {
  summary: string;
  diff: GraphDiff;
  state: ProposalState;
  /**
   * Why this proposal cannot be applied, when it cannot: the graph moved under
   * it, it was replayed from an earlier session, or it would change nothing.
   * Present means the Apply button is not offered at all rather than offered
   * and refused.
   */
  blocked?: string;
  /** The host's refusal, when an Apply was attempted and came back rejected. */
  error?: string;
  onApply: () => void;
  onDismiss: () => void;
}

export function ProposalCard({
  summary,
  diff,
  state,
  blocked,
  error,
  onApply,
  onDismiss,
}: Props) {
  const settled = state === "applied" || state === "dismissed";
  return (
    <div
      data-testid="workflow-proposal"
      data-state={state}
      className={cn(
        "rounded-lg border bg-background p-2 text-[11px] leading-snug",
        settled && "opacity-60",
      )}
    >
      <div className="flex items-start justify-between gap-2">
        <p className="font-medium text-foreground">{summary}</p>
        {state === "applied" && (
          <span className="inline-flex shrink-0 items-center gap-1 text-emerald-600 dark:text-emerald-400">
            <Check className="size-3" /> Applied
          </span>
        )}
        {state === "dismissed" && <span className="shrink-0 text-muted-foreground">Dismissed</span>}
      </div>

      <ul className="mt-2 space-y-1">
        {diff.nodes.map((node) => (
          <li key={`n-${node.id}`}>
            <NodeLine node={node} />
          </li>
        ))}
        {diff.edges.map((edge) => (
          <li
            key={`e-${edge.change}-${edge.from}-${edge.to}`}
            className="flex items-center gap-1 text-muted-foreground"
          >
            {edge.change === "added" ? (
              <Plus className="size-3 shrink-0 text-emerald-600 dark:text-emerald-400" />
            ) : (
              <Minus className="size-3 shrink-0 text-destructive" />
            )}
            <span className="font-mono">{edge.from}</span>
            <ArrowRight className="size-3 shrink-0" aria-hidden />
            <span className="font-mono">{edge.to}</span>
          </li>
        ))}
        {diff.nodes.length === 0 && diff.edges.length === 0 && (
          <li className="text-muted-foreground">
            This would change nothing about the workflow as it stands.
          </li>
        )}
      </ul>

      {blocked && !settled && (
        <Alert className="mt-2 py-1.5">
          <AlertTriangle className="size-3" />
          <AlertDescription className="text-[11px] leading-snug">{blocked}</AlertDescription>
        </Alert>
      )}

      {error && (
        <Alert variant="destructive" className="mt-2 py-1.5">
          <AlertTriangle className="size-3" />
          <AlertDescription className="text-[11px] leading-snug">{error}</AlertDescription>
        </Alert>
      )}

      {!settled && (
        <div className="mt-2 flex items-center gap-2">
          <Button
            size="sm"
            className="h-7 px-2 text-[11px]"
            disabled={state === "applying" || Boolean(blocked)}
            onClick={onApply}
            data-testid="workflow-proposal-apply"
          >
            {state === "applying" ? (
              <Loader2 className="mr-1 size-3 animate-spin" />
            ) : (
              <Check className="mr-1 size-3" />
            )}
            Apply
          </Button>
          <Button
            size="sm"
            variant="ghost"
            className="h-7 px-2 text-[11px]"
            disabled={state === "applying"}
            onClick={onDismiss}
            data-testid="workflow-proposal-dismiss"
          >
            <X className="mr-1 size-3" />
            Dismiss
          </Button>
        </div>
      )}
    </div>
  );
}

/** One step's line: added, removed, or changed with the fields that moved. */
function NodeLine({ node }: { node: NodeChange }) {
  if (node.change === "added") {
    return (
      <span className="block text-muted-foreground">
        <span className="flex items-start gap-1">
          <Plus className="mt-px size-3 shrink-0 text-emerald-600 dark:text-emerald-400" />
          <span>
            new step <span className="font-mono">{node.id}</span>{" "}
            <span className="text-foreground">{node.name}</span>
          </span>
        </span>
        {/* A new step's whole payload, not just its name: its kind, and any
            schedule, config or approval flag it arrives with, each of which
            changes what the workflow does. Reviewing a name and applying a
            scheduled auto-approving node is the gap this closes. */}
        <span className="mt-0.5 ml-4 block space-y-0.5">
          {node.fields.map((field) => (
            <span key={field.field} className="block">
              <span className="font-medium text-foreground">{field.field}</span>:{" "}
              <span className="text-foreground">{field.after}</span>
            </span>
          ))}
        </span>
      </span>
    );
  }
  if (node.change === "removed") {
    return (
      <span className="flex items-start gap-1 text-muted-foreground">
        <Minus className="mt-px size-3 shrink-0 text-destructive" />
        <span>
          remove <span className="font-mono">{node.id}</span>{" "}
          <span className="text-foreground">{node.name}</span>
        </span>
      </span>
    );
  }
  return (
    <span className="block text-muted-foreground">
      <span className="flex items-start gap-1">
        <ArrowRight className="mt-px size-3 shrink-0" aria-hidden />
        <span>
          <span className="font-mono">{node.id}</span>{" "}
          <span className="text-foreground">{node.name}</span>
        </span>
      </span>
      <span className="mt-0.5 ml-4 block space-y-0.5">
        {node.fields.map((field) => (
          <span key={field.field} className="block">
            <span className="font-medium text-foreground">{field.field}</span>:{" "}
            <span className="line-through">{field.before}</span>{" "}
            <ArrowRight className="inline size-3 align-[-2px]" aria-hidden />{" "}
            <span className="text-foreground">{field.after}</span>
          </span>
        ))}
      </span>
    </span>
  );
}
