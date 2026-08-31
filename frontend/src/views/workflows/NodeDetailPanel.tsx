// The read-only node inspector overlaid on the canvas when a node is clicked.
//
// Extracted verbatim from `WorkflowsView.tsx` (issue #303).

import { useEffect, type ReactNode } from "react";
import { X } from "lucide-react";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";

import { nodeKindLabel, type WorkflowNode as WorkflowNodeModel } from "@/api/workflows";
import type { TeamMemberDto } from "@/api/types";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { nodeKindMeta } from "@/lib/workflow-sample";

import { type NodeOutputView, isRecord, parseNodeMessages } from "./run-output";

/** A read-only inspector for a single graph node, overlaid on the canvas when
 * the operator clicks a node. Surfaces the fields already on the wire from
 * `GET …/workflows/{wid}`: kind, name, summary, the assigned agent (agent
 * nodes), the trigger's cron schedule (trigger nodes, issue #169), and any
 * kind-specific config / error-handling policy. */
export function NodeDetailPanel({
  node,
  roster = [],
  output,
  onClose,
}: {
  node: WorkflowNodeModel;
  /** Roster names for resolving an agent node's machine id. */
  roster?: TeamMemberDto[];
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
  const kindLabel = nodeKindLabel(node.kind);
  const teammate = node.agent ? roster.find((member) => member.id === node.agent) : undefined;
  const teammateName = teammate ? teammate.name?.trim() || teammate.role : undefined;
  const hasConfig =
    node.config !== undefined && node.config !== null &&
    !(typeof node.config === "object" && Object.keys(node.config as object).length === 0);

  useEffect(() => {
    const closeOnEscape = (event: KeyboardEvent) => {
      if (event.key === "Escape") onClose();
    };
    window.addEventListener("keydown", closeOnEscape);
    return () => window.removeEventListener("keydown", closeOnEscape);
  }, [onClose]);

  return (
    <div
      // Issue #1231: the overlay's geometry is asserted, not assumed —
      // `RevealSelectedNode` pans the canvas so the inspected node clears
      // this box, and the e2e spec measures both.
      data-testid="workflow-node-detail"
      className="absolute right-3 top-3 bottom-3 z-10 flex w-72 flex-col overflow-hidden rounded-xl border bg-card/95 shadow-lg backdrop-blur sm:w-80"
    >
      <div className="flex items-start justify-between gap-2 border-b px-3 py-2">
        <div className="flex min-w-0 items-center gap-2">
          <span className="text-base leading-none" aria-hidden>
            {meta.emoji}
          </span>
          <div className="min-w-0">
            <div className="truncate text-sm font-semibold">{node.name}</div>
            <div className="truncate text-2xs text-muted-foreground">{kindLabel}</div>
          </div>
        </div>
        <Button
          variant="ghost"
          size="icon"
          className="-mr-1 size-7"
          onClick={onClose}
          aria-label="Close"
        >
          <X className="size-4" />
        </Button>
      </div>

      <div className="min-h-0 flex-1 space-y-3 overflow-auto px-3 py-3 text-sm">
        <div className="flex flex-wrap items-center gap-1.5">
          <Badge variant="outline" className="font-normal">
            {kindLabel}
          </Badge>
          {node.requiresApproval && (
            <Badge variant="outline" className="border-status-blocked/40 bg-status-blocked-soft font-normal">
              requires approval
            </Badge>
          )}
          {/* Issue #850. Only `false` is a statement — absent means "repeats",
              which is the default and not worth a badge. Says what happens
              rather than naming the field, because the operator's question is
              what approving will do. */}
          {node.repeatable === false && (
            <Badge
              variant="outline"
              className="border-status-blocked/40 bg-status-blocked-soft font-normal"
              data-testid="node-not-repeated"
            >
              not repeated on approval
            </Badge>
          )}
          {node.schedule && (
            <Badge variant="outline" className="border-status-running/40 bg-status-running-soft font-normal">
              scheduled
            </Badge>
          )}
        </div>

        <DetailField label="Node ID">
          <p className="font-mono text-xs text-muted-foreground">{node.id}</p>
        </DetailField>

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
          <DetailField label="Assigned teammate">
            <p className="text-sm">{teammateName ?? node.agent}</p>
            {teammateName && teammateName !== node.agent && (
              <p className="font-mono text-3xs text-muted-foreground">Roster ID: {node.agent}</p>
            )}
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

        {/* Issue #1866, found in the #1937 boundary sweep: a run-safety gate
            an operator declared must be visible here, the same way onError/
            retry above are — otherwise the panel silently claims a node has
            "no extra details" while the runtime still enforces a gate on it. */}
        {node.postcondition && (
          <DetailField label="Postcondition">
            <pre className="overflow-auto rounded-lg border bg-muted/40 p-2 font-mono text-2xs leading-snug">
              {JSON.stringify(node.postcondition, null, 2)}
            </pre>
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
          !node.requiresApproval &&
          !node.postcondition &&
          // Issue #850: `repeatable === false` renders the "not repeated on
          // approval" badge above, so a node whose only detail is that
          // declaration must not also claim it has no extra details.
          node.repeatable !== false && (
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
  const artifacts = parseRunArtifacts(output.value);
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
        ) : artifacts.length === 0 ? (
          <p className="text-xs text-muted-foreground" data-testid="node-output-none">
            This node produced no readable text — see the raw value below.
          </p>
        ) : null}
        {artifacts.length > 0 && (
          <div className="space-y-1.5" data-testid="node-output-artifacts">
            <p className="text-3xs uppercase tracking-wide text-muted-foreground">
              Artifacts
            </p>
            {artifacts.map((artifact) => (
              <a
                key={`${artifact.workspaceNodeId}-${artifact.source}`}
                className="block rounded-md border bg-muted/30 px-2 py-1.5 hover:border-primary/40"
                href={`#/workspace/${encodeURIComponent(artifact.workspaceNodeId)}`}
                data-testid="node-output-artifact"
              >
                <span className="block truncate text-xs font-medium text-primary">
                  {artifact.title}
                </span>
                <span className="block truncate text-3xs text-muted-foreground">
                  {artifact.source}
                </span>
              </a>
            ))}
          </div>
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

interface RunArtifactView {
  source: string;
  title: string;
  workspaceNodeId: string;
}

/** Reads the host's card-less run-artifact rows defensively. */
function parseRunArtifacts(raw: unknown): RunArtifactView[] {
  if (!isRecord(raw) || !Array.isArray(raw.artifacts)) return [];
  return raw.artifacts.flatMap((row) => {
    if (!isRecord(row)) return [];
    const source = typeof row.source === "string" ? row.source : "";
    const title = typeof row.title === "string" ? row.title : source;
    const workspaceNodeId =
      typeof row.workspaceNodeId === "string" ? row.workspaceNodeId : "";
    return source && title && workspaceNodeId
      ? [{ source, title, workspaceNodeId }]
      : [];
  });
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
