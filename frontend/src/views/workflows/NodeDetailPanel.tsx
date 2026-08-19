// The read-only node inspector overlaid on the canvas when a node is clicked.
//
// Extracted verbatim from `WorkflowsView.tsx` (issue #303).

import type { ReactNode } from "react";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";

import type { WorkflowNode as WorkflowNodeModel } from "@/api/workflows";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { nodeKindMeta } from "@/lib/workflow-sample";

import { type NodeOutputView, parseNodeMessages } from "./run-output";

/** A read-only inspector for a single graph node, overlaid on the canvas when
 * the operator clicks a node. Surfaces the fields already on the wire from
 * `GET …/workflows/{wid}`: kind, name, summary, the assigned agent (agent
 * nodes), the trigger's cron schedule (trigger nodes, issue #169), and any
 * kind-specific config / error-handling policy. */
export function NodeDetailPanel({
  node,
  output,
  onClose,
}: {
  node: WorkflowNodeModel;
  /**
   * This node's output on the run being inspected (issue #596). `undefined`
   * means "not inspecting a run" — the panel then shows only the node's static
   * config, exactly as before. When present, an Output section renders what the
   * node produced (or a loading / empty state).
   */
  output?: NodeOutputView;
  onClose: () => void;
}) {
  const meta = nodeKindMeta(node.kind);
  const hasConfig =
    node.config !== undefined && node.config !== null &&
    !(typeof node.config === "object" && Object.keys(node.config as object).length === 0);

  return (
    <div className="absolute right-3 top-3 bottom-3 z-10 flex w-72 flex-col overflow-hidden rounded-xl border bg-card/95 shadow-lg backdrop-blur sm:w-80">
      <div className="flex items-start justify-between gap-2 border-b px-3 py-2">
        <div className="flex min-w-0 items-center gap-2">
          <span className="text-base leading-none" aria-hidden>
            {meta.emoji}
          </span>
          <div className="min-w-0">
            <div className="truncate text-sm font-semibold">{node.name}</div>
            <div className="truncate text-2xs text-muted-foreground">{node.id}</div>
          </div>
        </div>
        <Button variant="ghost" size="sm" className="-mr-1 h-7 px-2" onClick={onClose}>
          Close
        </Button>
      </div>

      <div className="min-h-0 flex-1 space-y-3 overflow-auto px-3 py-3 text-sm">
        <div className="flex flex-wrap items-center gap-1.5">
          <Badge variant="outline" className="font-normal">
            {node.kind}
          </Badge>
          {node.requiresApproval && (
            <Badge variant="outline" className="border-status-blocked/40 bg-status-blocked-soft font-normal">
              requires approval
            </Badge>
          )}
          {node.schedule && (
            <Badge variant="outline" className="border-status-running/40 bg-status-running-soft font-normal">
              scheduled
            </Badge>
          )}
        </div>

        {/* A saved schedule must be visible, not write-only — otherwise an
            operator cannot tell a self-running workflow from a manual one. */}
        {node.schedule && (
          <DetailField label="Schedule">
            <p className="font-mono text-xs">{node.schedule}</p>
            <p className="text-3xs text-muted-foreground">5-field cron, UTC.</p>
          </DetailField>
        )}

        {node.summary && (
          <DetailField label="Summary">
            <p className="text-sm leading-snug">{node.summary}</p>
          </DetailField>
        )}

        {node.agent && (
          <DetailField label="Assigned agent">
            <p className="font-mono text-xs">{node.agent}</p>
          </DetailField>
        )}

        {hasConfig && (
          <DetailField label="Config">
            <pre className="overflow-auto rounded-lg border bg-muted/40 p-2 font-mono text-2xs leading-snug">
              {JSON.stringify(node.config, null, 2)}
            </pre>
          </DetailField>
        )}

        {node.onError && (
          <DetailField label="On error">
            <p className="font-mono text-xs">{node.onError}</p>
          </DetailField>
        )}

        {node.retry && (
          <DetailField label="Retry">
            <pre className="overflow-auto rounded-lg border bg-muted/40 p-2 font-mono text-2xs leading-snug">
              {JSON.stringify(node.retry, null, 2)}
            </pre>
          </DetailField>
        )}

        {node.destination && (
          <DetailField label="Destination">
            <p className="text-sm leading-snug">{describeDestination(node.destination)}</p>
          </DetailField>
        )}

        {/* Issue #596: what this node actually produced on the run being
            inspected — the make.com per-node output view. Only rendered when a
            run is being inspected (a live run's clicked node, or a past run
            reopened from History). */}
        {output && <OutputSection output={output} />}

        {!output &&
          !node.summary &&
          !node.agent &&
          !hasConfig &&
          !node.onError &&
          !node.retry &&
          !node.schedule &&
          !node.destination &&
          !node.requiresApproval && (
            <p className="text-xs text-muted-foreground">
              This node has no extra details beyond its kind and name.
            </p>
          )}
      </div>
    </div>
  );
}

/** Where an output node's report goes, in a sentence. `owner` deliberately has
 * no target to show — it resolves to the company's admins server-side, which is
 * exactly why an author can't point it at an outsider. */
function describeDestination(destination: NonNullable<WorkflowNodeModel["destination"]>): string {
  switch (destination.kind) {
    case "owner":
      return "Reports to the company's admins.";
    case "email":
      return `Emails ${destination.target ?? "(no address)"}.`;
    case "channel":
      return `Posts to the ${destination.target ?? "(unnamed)"} channel.`;
    default:
      return `${destination.kind}${destination.target ? ` → ${destination.target}` : ""}`;
  }
}

/**
 * The Output section of the node inspector (issue #596): what this node produced
 * on the run being inspected — markdown-rendered agent/tool messages, the raw
 * value behind a toggle, a "truncated" badge when the durable snapshot was
 * clipped, a "partial capture" badge when the run failed or blocked (issue
 * #1008), and an explicit empty state for a run that genuinely has no snapshot.
 *
 * Exported for the unit test that pins the badges and the empty state without
 * standing up the whole panel.
 */
export function OutputSection({ output }: { output: NodeOutputView }) {
  if (output.state === "loading") {
    return (
      <DetailField label="Output">
        <p className="text-xs text-muted-foreground">Loading output…</p>
      </DetailField>
    );
  }

  if (output.state === "unavailable") {
    return (
      <DetailField label="Output">
        <p className="text-xs text-muted-foreground" data-testid="node-output-empty">
          No output for this node — this run predates output capture, or the node
          produced none.
        </p>
      </DetailField>
    );
  }

  const messages = parseNodeMessages(output.value);
  return (
    <DetailField label="Output">
      <div className="space-y-2" data-testid="node-output">
        {output.partial && (
          <Badge
            variant="outline"
            className="border-status-blocked/40 bg-status-blocked-soft font-normal"
            data-testid="node-output-partial"
          >
            partial capture — run failed or blocked
          </Badge>
        )}
        {output.truncated && (
          <Badge
            variant="outline"
            className="border-status-blocked/40 bg-status-blocked-soft font-normal"
            data-testid="node-output-truncated"
          >
            truncated — clipped to fit
          </Badge>
        )}
        {messages.length > 0 ? (
          messages.map((m, i) => (
            <div key={i} className={i > 0 ? "border-t pt-2" : undefined}>
              {m.agentRef && (
                <p className="mb-1 text-3xs uppercase tracking-wide text-muted-foreground">
                  {m.agentRef}
                </p>
              )}
              {m.text ? (
                <div className="prose prose-sm max-w-none dark:prose-invert">
                  <ReactMarkdown remarkPlugins={[remarkGfm]}>{m.text}</ReactMarkdown>
                </div>
              ) : (
                <p className="text-sm text-muted-foreground">—</p>
              )}
            </div>
          ))
        ) : (
          <p className="text-xs text-muted-foreground" data-testid="node-output-none">
            This node produced no readable text — see the raw value below.
          </p>
        )}
        <details>
          <summary className="cursor-pointer text-xs text-muted-foreground">
            Show raw output
          </summary>
          <pre className="mt-1 overflow-auto rounded-lg border bg-muted/40 p-2 font-mono text-2xs leading-snug">
            {JSON.stringify(output.value, null, 2)}
          </pre>
        </details>
      </div>
    </DetailField>
  );
}

/** A labelled block inside the node inspector. */
function DetailField({ label, children }: { label: string; children: ReactNode }) {
  return (
    <div className="space-y-1">
      <p className="text-3xs uppercase tracking-wide text-muted-foreground">{label}</p>
      {children}
    </div>
  );
}
