// The read-only node inspector overlaid on the canvas when a node is clicked.
//
// Extracted verbatim from `WorkflowsView.tsx` (issue #303).

import type { ReactNode } from "react";

import type { WorkflowNode as WorkflowNodeModel } from "@/api/workflows";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { nodeKindMeta } from "@/lib/workflow-sample";

/** A read-only inspector for a single graph node, overlaid on the canvas when
 * the operator clicks a node. Surfaces the fields already on the wire from
 * `GET …/workflows/{wid}`: kind, name, summary, the assigned agent (agent
 * nodes), the trigger's cron schedule (trigger nodes, issue #169), and any
 * kind-specific config / error-handling policy. */
export function NodeDetailPanel({
  node,
  onClose,
}: {
  node: WorkflowNodeModel;
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
            <div className="truncate text-[11px] text-muted-foreground">{node.id}</div>
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
            <Badge variant="outline" className="border-amber-500/40 bg-amber-500/10 font-normal">
              requires approval
            </Badge>
          )}
          {node.schedule && (
            <Badge variant="outline" className="border-sky-500/40 bg-sky-500/10 font-normal">
              scheduled
            </Badge>
          )}
        </div>

        {/* A saved schedule must be visible, not write-only — otherwise an
            operator cannot tell a self-running workflow from a manual one. */}
        {node.schedule && (
          <DetailField label="Schedule">
            <p className="font-mono text-xs">{node.schedule}</p>
            <p className="text-[10px] text-muted-foreground">5-field cron, UTC.</p>
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
            <pre className="overflow-auto rounded-lg border bg-muted/40 p-2 font-mono text-[11px] leading-snug">
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
            <pre className="overflow-auto rounded-lg border bg-muted/40 p-2 font-mono text-[11px] leading-snug">
              {JSON.stringify(node.retry, null, 2)}
            </pre>
          </DetailField>
        )}

        {node.destination && (
          <DetailField label="Destination">
            <p className="text-sm leading-snug">{describeDestination(node.destination)}</p>
          </DetailField>
        )}

        {!node.summary &&
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

/** A labelled block inside the node inspector. */
function DetailField({ label, children }: { label: string; children: ReactNode }) {
  return (
    <div className="space-y-1">
      <p className="text-[10px] uppercase tracking-wide text-muted-foreground">{label}</p>
      {children}
    </div>
  );
}
