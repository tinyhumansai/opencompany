import { type ReactNode, useCallback, useEffect, useMemo, useState } from "react";
import {
  Background,
  BackgroundVariant,
  Controls,
  type Edge,
  MiniMap,
  type Node,
  ReactFlow,
} from "@xyflow/react";
import "@xyflow/react/dist/style.css";
import { useTheme } from "next-themes";
import { Loader2, Play, Plus } from "lucide-react";
import { toast } from "sonner";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";

import {
  getWorkflow,
  listWorkflows,
  runWorkflow,
  type WorkflowGraph,
  type WorkflowNode as WorkflowNodeModel,
  type WorkflowRunResult,
  type WorkflowSummary,
} from "@/api/workflows";
import type { OpenCompanyClient } from "@/api/client";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { Skeleton } from "@/components/ui/skeleton";
import { WorkflowNode } from "@/components/workflow-node";
import { WorkflowCreateDialog } from "@/views/WorkflowCreateDialog";
import { nodeKindMeta, type WorkflowNodeData } from "@/lib/workflow-sample";

const NODE_TYPES = { oc: WorkflowNode };

/** Horizontal gap between layers and vertical gap between nodes in a layer. */
const COL_GAP = 300;
const ROW_GAP = 150;

/**
 * The live Workflows canvas. Reads the company's saved graphs from the host's
 * `…/workflows` routes, lets the operator pick one, renders its real nodes and
 * edges (auto-laid-out left→right by longest-path depth, since saved graphs
 * carry no coordinates), and runs it via `…/workflows/{wid}/run` — surfacing the
 * engine's final output and any nodes left pending approval.
 */
export function WorkflowsView({
  client,
  company,
}: {
  client: OpenCompanyClient;
  company: string | null;
}) {
  const { resolvedTheme } = useTheme();
  const [workflows, setWorkflows] = useState<WorkflowSummary[]>([]);
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [graph, setGraph] = useState<WorkflowGraph | null>(null);
  const [loadingList, setLoadingList] = useState(true);
  const [loadingGraph, setLoadingGraph] = useState(false);
  const [running, setRunning] = useState(false);
  const [result, setResult] = useState<WorkflowRunResult | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [createOpen, setCreateOpen] = useState(false);
  const [selectedNodeId, setSelectedNodeId] = useState<string | null>(null);

  // Load the workflow list once, and auto-select the first entry.
  useEffect(() => {
    let live = true;
    (async () => {
      try {
        const rows = await listWorkflows(client, company);
        if (!live) return;
        setWorkflows(rows);
        // Keep the selection only if it still exists in the freshly loaded list
        // (this effect also reruns on company change) — otherwise a stale id
        // from the previous company would fetch the wrong/nonexistent graph.
        setSelectedId((prev) =>
          prev && rows.some((r) => r.id === prev) ? prev : (rows[0]?.id ?? null),
        );
        setError(null);
      } catch (e) {
        if (!live) return;
        setError(e instanceof Error ? e.message : "could not load workflows");
      } finally {
        if (live) setLoadingList(false);
      }
    })();
    return () => {
      live = false;
    };
  }, [client, company]);

  // Fetch the selected workflow's full graph.
  useEffect(() => {
    if (!selectedId) {
      setGraph(null);
      return;
    }
    let live = true;
    setLoadingGraph(true);
    setResult(null);
    setSelectedNodeId(null);
    (async () => {
      try {
        const g = await getWorkflow(client, company, selectedId);
        if (!live) return;
        setGraph(g);
        setError(null);
      } catch (e) {
        if (!live) return;
        setGraph(null);
        setError(e instanceof Error ? e.message : "could not load the workflow graph");
      } finally {
        if (live) setLoadingGraph(false);
      }
    })();
    return () => {
      live = false;
    };
  }, [client, company, selectedId]);

  const run = useCallback(async () => {
    if (!selectedId) return;
    setRunning(true);
    try {
      const res = await runWorkflow(client, company, selectedId);
      setResult(res);
      toast.success("Workflow ran.");
    } catch (e) {
      toast.error(e instanceof Error ? e.message : "could not run the workflow");
    } finally {
      setRunning(false);
    }
  }, [client, company, selectedId]);

  // The creator posts the full graph back, so the new entry can be spliced
  // straight into the list and selected — no extra round trip to re-list.
  const handleCreated = useCallback((created: WorkflowGraph) => {
    setWorkflows((prev) => {
      const rest = prev.filter((w) => w.id !== created.id);
      return [...rest, { id: created.id, name: created.name, description: created.description }].sort(
        (a, b) => a.name.localeCompare(b.name),
      );
    });
    setSelectedId(created.id);
    toast.success("Workflow created.");
  }, []);

  const { nodes, edges } = useMemo(() => (graph ? layout(graph) : { nodes: [], edges: [] }), [graph]);

  const selected = workflows.find((w) => w.id === selectedId) ?? null;

  // The full node model (kind/name/summary/agent/config) for the clicked node,
  // looked up from the loaded graph so the inspector shows fields the laid-out
  // canvas node data doesn't carry (agent, config, …).
  const selectedNode = useMemo(
    () => graph?.nodes.find((n) => n.id === selectedNodeId) ?? null,
    [graph, selectedNodeId],
  );

  const onNodeClick = useCallback((_: unknown, node: Node) => {
    setSelectedNodeId(node.id);
  }, []);

  return (
    <div className="flex flex-1 flex-col overflow-hidden">
      <div className="flex flex-wrap items-center justify-between gap-2 border-b px-4 py-3">
        <div className="flex min-w-0 items-center gap-2">
          <h2 className="text-sm font-semibold">Workflows</h2>
          <Badge variant="secondary">{workflows.length}</Badge>
          {selected?.description && (
            <p className="hidden truncate text-xs text-muted-foreground sm:block">
              {selected.description}
            </p>
          )}
        </div>
        <div className="flex items-center gap-2">
          <Select
            value={selectedId ?? undefined}
            onValueChange={(v) => setSelectedId(v)}
            disabled={loadingList || workflows.length === 0}
          >
            <SelectTrigger className="h-8 w-56">
              <SelectValue placeholder={loadingList ? "Loading…" : "Pick a workflow"} />
            </SelectTrigger>
            <SelectContent>
              {workflows.map((w) => (
                <SelectItem key={w.id} value={w.id}>
                  {w.name}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
          <Button size="sm" onClick={() => void run()} disabled={!selectedId || running || loadingGraph}>
            {running ? (
              <Loader2 className="mr-1.5 size-4 animate-spin" />
            ) : (
              <Play className="mr-1.5 size-4" />
            )}
            Run
          </Button>
          <Button size="sm" variant="outline" onClick={() => setCreateOpen(true)}>
            <Plus className="mr-1.5 size-4" />
            New workflow
          </Button>
        </div>
      </div>

      {error && (
        <div className="px-4 pt-3">
          <Alert variant="destructive">
            <AlertDescription>{error}</AlertDescription>
          </Alert>
        </div>
      )}

      <div className="relative flex-1">
        {loadingList || loadingGraph ? (
          <div className="absolute inset-0 p-4">
            <Skeleton className="h-full w-full rounded-xl" />
          </div>
        ) : !selectedId ? (
          <div className="flex h-full flex-col items-center justify-center gap-3 px-4 text-center text-sm text-muted-foreground">
            <p>This company has no saved workflows yet.</p>
            <Button size="sm" variant="outline" onClick={() => setCreateOpen(true)}>
              <Plus className="mr-1.5 size-4" />
              New workflow
            </Button>
          </div>
        ) : (
          <>
            <ReactFlow
              nodes={nodes}
              edges={edges}
              nodeTypes={NODE_TYPES}
              colorMode={resolvedTheme === "dark" ? "dark" : "light"}
              fitView
              fitViewOptions={{ padding: 0.2 }}
              nodesDraggable={false}
              nodesConnectable={false}
              elementsSelectable
              onNodeClick={onNodeClick}
              onPaneClick={() => setSelectedNodeId(null)}
              proOptions={{ hideAttribution: true }}
            >
              <Background variant={BackgroundVariant.Dots} gap={20} size={1} />
              <Controls showInteractive={false} />
              <MiniMap pannable zoomable className="!hidden sm:!block" />
            </ReactFlow>
            {selectedNode && (
              <NodeDetailPanel node={selectedNode} onClose={() => setSelectedNodeId(null)} />
            )}
          </>
        )}
      </div>

      {result && (
        <RunResultPanel result={result} graph={graph} onClose={() => setResult(null)} />
      )}

      <WorkflowCreateDialog
        client={client}
        company={company}
        open={createOpen}
        onOpenChange={setCreateOpen}
        onCreated={handleCreated}
      />
    </div>
  );
}

/** A read-only inspector for a single graph node, overlaid on the canvas when
 * the operator clicks a node. Surfaces the fields already on the wire from
 * `GET …/workflows/{wid}`: kind, name, summary, the assigned agent (agent
 * nodes), and any kind-specific config / error-handling policy. */
function NodeDetailPanel({
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
        </div>

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

        {!node.summary &&
          !node.agent &&
          !hasConfig &&
          !node.onError &&
          !node.retry &&
          !node.requiresApproval && (
            <p className="text-xs text-muted-foreground">
              This node has no extra details beyond its kind and name.
            </p>
          )}
      </div>
    </div>
  );
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

/** The run-output drawer: one readable card per executed node (the producing
 * agent and its reply, markdown-rendered) plus the branch each condition node
 * took, any nodes left pending approval, and the raw engine state collapsed
 * behind a toggle. Falls back to a raw JSON dump when the output doesn't match
 * the expected per-node shape. */
function RunResultPanel({
  result,
  graph,
  onClose,
}: {
  result: WorkflowRunResult;
  graph: WorkflowGraph | null;
  onClose: () => void;
}) {
  const nodeResults = useMemo(
    () => parseRunNodes(result.output, graph),
    [result.output, graph],
  );

  return (
    <div className="border-t bg-card/60">
      <div className="flex items-center justify-between px-4 py-2">
        <div className="flex items-center gap-2">
          <span className="text-sm font-medium">Run result</span>
          {result.pendingApprovals.length > 0 && (
            <Badge variant="outline" className="border-amber-500/40 bg-amber-500/10">
              {result.pendingApprovals.length} pending approval
              {result.pendingApprovals.length === 1 ? "" : "s"}
            </Badge>
          )}
        </div>
        <Button variant="ghost" size="sm" onClick={onClose}>
          Dismiss
        </Button>
      </div>
      <div className="max-h-72 overflow-auto px-4 pb-3">
        {result.pendingApprovals.length > 0 && (
          <p className="mb-2 text-xs text-muted-foreground">
            Waiting on: {result.pendingApprovals.join(", ")}
          </p>
        )}

        {nodeResults && nodeResults.length > 0 ? (
          <div className="mb-2 space-y-2">
            {nodeResults.map((n) => (
              <NodeResultCard key={n.id} node={n} />
            ))}
          </div>
        ) : (
          <p className="mb-2 text-xs text-muted-foreground">
            The run finished, but its output didn't match the expected node
            shape — see the raw output below.
          </p>
        )}

        <details open={!nodeResults || nodeResults.length === 0}>
          <summary className="cursor-pointer text-xs text-muted-foreground">
            Show raw engine output
          </summary>
          <pre className="mt-1 rounded-lg border bg-muted/40 p-2 font-mono text-[11px] leading-snug">
            {JSON.stringify(result.output, null, 2)}
          </pre>
        </details>
      </div>
    </div>
  );
}

/** One node's readable result: its name, the producing agent, and its reply
 * (markdown-rendered). Falls back to a subtle placeholder / the branch it took
 * when it produced no text (e.g. a trigger or a condition node). */
function NodeResultCard({ node }: { node: NodeResult }) {
  return (
    <div className="rounded-lg border bg-background/40 p-2">
      <div className="mb-1 flex items-center gap-2">
        <span className="truncate text-xs font-medium">{node.name}</span>
        {node.port !== null && (
          <Badge variant="outline" className="h-4 px-1.5 text-[10px] font-normal">
            branch: {node.port}
          </Badge>
        )}
      </div>
      {node.messages.map((m, i) => (
        <div key={i} className={i > 0 ? "mt-2 border-t pt-2" : undefined}>
          {m.agentRef && (
            <p className="mb-1 text-[10px] uppercase tracking-wide text-muted-foreground">
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
      ))}
      {node.messages.length === 0 && (
        <p className="text-sm text-muted-foreground">—</p>
      )}
    </div>
  );
}

/** Lays a saved graph out left→right by longest-path depth, stacking siblings
 * vertically within each layer. Cycles are bounded by an iteration cap, so a
 * back edge never loops forever. */
function layout(graph: WorkflowGraph): { nodes: Node<WorkflowNodeData>[]; edges: Edge[] } {
  const depth = new Map<string, number>(graph.nodes.map((n) => [n.id, 0]));
  for (let i = 0; i < graph.nodes.length; i++) {
    let changed = false;
    for (const e of graph.edges) {
      const d = (depth.get(e.from) ?? 0) + 1;
      if (d > (depth.get(e.to) ?? 0)) {
        depth.set(e.to, d);
        changed = true;
      }
    }
    if (!changed) break;
  }

  const rowInLayer = new Map<number, number>();
  const nodes: Node<WorkflowNodeData>[] = graph.nodes.map((n) => {
    const layer = depth.get(n.id) ?? 0;
    const row = rowInLayer.get(layer) ?? 0;
    rowInLayer.set(layer, row + 1);
    const meta = nodeKindMeta(n.kind);
    return {
      id: n.id,
      type: "oc",
      position: { x: layer * COL_GAP, y: row * ROW_GAP },
      data: {
        kind: n.kind,
        name: n.name,
        // Agent nodes surface their roster id; otherwise the node's summary.
        summary: n.summary ?? (n.agent ? `Agent: ${n.agent}` : ""),
        emoji: meta.emoji,
        color: meta.color,
      },
    };
  });

  const edges: Edge[] = graph.edges.map((e, i) => ({
    id: `${e.from}-${e.to}-${i}`,
    source: e.from,
    target: e.to,
    label: e.label,
    animated: true,
  }));

  return { nodes, edges };
}

/** A single agent reply extracted from a node's `items[].json`. */
interface NodeMessage {
  text: string | null;
  agentRef: string | null;
}

/** One node's readable, shape-checked result, ready to render. */
interface NodeResult {
  id: string;
  name: string;
  /** The condition branch taken (`null` when the node isn't a branch point). */
  port: string | null;
  messages: NodeMessage[];
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null;
}

/** A non-empty trimmed string, else `null` (defensive against non-strings). */
function nonEmptyString(value: unknown): string | null {
  return typeof value === "string" && value.trim() ? value : null;
}

/** Pull a field from an item's `json`, preferring the OUTERMOST value and
 * falling back to the NESTED `json.json.<key>` the engine sometimes emits.
 * Handles the observed shape where `json` carries both a top-level `text` and
 * a nested `json.json.text` — the outer one wins. */
function readNested(json: unknown, key: string): string | null {
  if (!isRecord(json)) return null;
  const outer = nonEmptyString(json[key]);
  if (outer) return outer;
  const inner = json.json;
  if (isRecord(inner)) return nonEmptyString(inner[key]);
  return null;
}

/** Safely parse the engine's run output into per-node results, ordered by the
 * loaded graph when available (falling back to the map's insertion order).
 * Returns `null` when `output` doesn't match the expected `{ nodes: {…} }`
 * shape, signalling the caller to fall back to the raw JSON dump. Every access
 * is guarded — `output` is typed `unknown` and older/edge runs may be a plain
 * string, missing `nodes`, or carry malformed node values. */
function parseRunNodes(
  output: unknown,
  graph: WorkflowGraph | null,
): NodeResult[] | null {
  if (!isRecord(output) || !isRecord(output.nodes)) {
    console.debug(
      "[WorkflowsView] run output missing a `nodes` map; showing raw JSON",
      output,
    );
    return null;
  }
  const nodes = output.nodes;

  // Order by the graph's node order when we have it, then append any node ids
  // present in the output but not in the graph (in the map's insertion order).
  const graphOrder = graph?.nodes.map((n) => n.id) ?? [];
  const orderedIds = [
    ...graphOrder.filter((id) => id in nodes),
    ...Object.keys(nodes).filter((id) => !graphOrder.includes(id)),
  ];

  const nameById = new Map(graph?.nodes.map((n) => [n.id, n.name]) ?? []);

  const results: NodeResult[] = orderedIds.map((id) => {
    const raw = nodes[id];
    const items = isRecord(raw) && Array.isArray(raw.items) ? raw.items : [];
    const messages: NodeMessage[] = items
      .map((item) => {
        const json = isRecord(item) ? item.json : undefined;
        return {
          text: readNested(json, "text"),
          agentRef: readNested(json, "agent_ref"),
        };
      })
      .filter((m) => m.text || m.agentRef);
    const port = isRecord(raw) ? nonEmptyString(raw.port) : null;
    return { id, name: nameById.get(id) ?? id, port, messages };
  });

  return results;
}
