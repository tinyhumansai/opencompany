// The workflow creator (issue #69): a plain form editor — not a drag canvas —
// that builds a `WorkflowGraph` and posts it via `createWorkflow`. Node kinds
// are restricted to the ones the engine actually executes today
// (`CREATABLE_NODE_KINDS`); `tool_call`/`http_request` stay off the palette
// until they're wired (see `src/workflows/caps.rs`).

import { useEffect, useId, useState } from "react";
import { Plus, Trash2 } from "lucide-react";

import {
  CREATABLE_NODE_KINDS,
  createWorkflow,
  type WorkflowEdge,
  type WorkflowGraph,
  type WorkflowNode,
} from "@/api/workflows";
import type { OpenCompanyClient } from "@/api/client";
import type { TeamMemberDto } from "@/api/types";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { Textarea } from "@/components/ui/textarea";

/** A node row being edited. `key` is a stable React key, independent of the
 * user-editable `id` field (which can be blank or duplicated mid-edit). */
interface DraftNode {
  key: string;
  id: string;
  kind: string;
  name: string;
  summary: string;
  agent: string;
}

interface DraftEdge {
  key: string;
  from: string;
  to: string;
  label: string;
}

let seq = 0;
function nextKey(): string {
  seq += 1;
  return `row-${seq}`;
}

function starterNodes(): DraftNode[] {
  return [{ key: nextKey(), id: "start", kind: "trigger", name: "Start", summary: "", agent: "" }];
}

/** A safe on-disk id: no slashes, no `..`, not empty — mirrors the host's
 * `safe_wid` check so a bad id fails fast in the form instead of round-tripping
 * to the server first. */
function isSafeId(id: string): boolean {
  return /^[A-Za-z0-9_-]+$/.test(id);
}

export function WorkflowCreateDialog({
  client,
  company,
  open,
  onOpenChange,
  onCreated,
}: {
  client: OpenCompanyClient;
  company: string | null;
  open: boolean;
  onOpenChange: (open: boolean) => void;
  onCreated: (graph: WorkflowGraph) => void;
}) {
  const [id, setId] = useState("");
  const [name, setName] = useState("");
  const [description, setDescription] = useState("");
  const [nodes, setNodes] = useState<DraftNode[]>(starterNodes());
  const [edges, setEdges] = useState<DraftEdge[]>([]);
  const [roster, setRoster] = useState<TeamMemberDto[]>([]);
  const [submitting, setSubmitting] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const formId = useId();

  // Reload the roster (for the agent-node picker) and reset the draft each
  // time the dialog opens, so a prior attempt never leaks into the next one.
  useEffect(() => {
    if (!open) return;
    setId("");
    setName("");
    setDescription("");
    setNodes(starterNodes());
    setEdges([]);
    setError(null);
    let live = true;
    (async () => {
      try {
        const team = await client.listTeam(company);
        if (live) setRoster(team);
      } catch {
        // No roster surface on this host — agent nodes fall back to a free-text
        // teammate id below.
        if (live) setRoster([]);
      }
    })();
    return () => {
      live = false;
    };
  }, [open, client, company]);

  function addNode() {
    setNodes((rows) => [
      ...rows,
      { key: nextKey(), id: "", kind: "agent", name: "", summary: "", agent: "" },
    ]);
  }

  function updateNode(key: string, fields: Partial<DraftNode>) {
    setNodes((rows) => rows.map((r) => (r.key === key ? { ...r, ...fields } : r)));
  }

  function removeNode(key: string) {
    const removed = nodes.find((n) => n.key === key);
    setNodes((rows) => rows.filter((r) => r.key !== key));
    // Drop any edge that pointed at the removed node's id — a dangling
    // reference would just bounce back from the server as a 400.
    if (removed?.id) {
      setEdges((rows) => rows.filter((e) => e.from !== removed.id && e.to !== removed.id));
    }
  }

  function addEdge() {
    setEdges((rows) => [...rows, { key: nextKey(), from: "", to: "", label: "" }]);
  }

  function updateEdge(key: string, fields: Partial<DraftEdge>) {
    setEdges((rows) => rows.map((r) => (r.key === key ? { ...r, ...fields } : r)));
  }

  function removeEdge(key: string) {
    setEdges((rows) => rows.filter((r) => r.key !== key));
  }

  /** Client-side validation, mirroring the host's checks so most mistakes
   * surface here instead of round-tripping to the server first. Returns the
   * first problem found, or `null` when the draft is postable. */
  function validate(): string | null {
    if (!id.trim()) return "Give the workflow an id.";
    if (!isSafeId(id.trim())) return "The id can only use letters, numbers, `_`, and `-`.";
    if (!name.trim()) return "Give the workflow a name.";
    if (nodes.length === 0) return "Add at least one node.";
    const ids = new Set<string>();
    for (const n of nodes) {
      if (!n.id.trim()) return "Every node needs an id.";
      if (ids.has(n.id.trim())) return `Node id \`${n.id}\` is used more than once.`;
      ids.add(n.id.trim());
      if (!n.name.trim()) return `Node \`${n.id}\` needs a name.`;
      if (n.kind === "agent" && !n.agent.trim()) {
        return `Node \`${n.id}\` is an agent node — pick who does it.`;
      }
    }
    const triggerCount = nodes.filter((n) => n.kind === "trigger").length;
    if (triggerCount !== 1) {
      return "A workflow needs exactly one trigger node to say what starts it.";
    }
    for (const e of edges) {
      if (!e.from || !e.to) return "Every edge needs a from-node and a to-node.";
      if (!ids.has(e.from)) return `Edge starts at \`${e.from}\`, which isn't one of the nodes.`;
      if (!ids.has(e.to)) return `Edge points to \`${e.to}\`, which isn't one of the nodes.`;
      if (e.from === e.to) return "An edge can't loop a node back to itself.";
    }
    return null;
  }

  async function submit() {
    const problem = validate();
    if (problem) {
      setError(problem);
      return;
    }
    setSubmitting(true);
    setError(null);
    const graph: WorkflowGraph = {
      id: id.trim(),
      name: name.trim(),
      description: description.trim() || undefined,
      nodes: nodes.map(
        (n): WorkflowNode => ({
          id: n.id.trim(),
          kind: n.kind,
          name: n.name.trim(),
          summary: n.summary.trim() || undefined,
          agent: n.kind === "agent" ? n.agent.trim() : undefined,
        }),
      ),
      edges: edges.map(
        (e): WorkflowEdge => ({
          from: e.from.trim(),
          to: e.to.trim(),
          label: e.label.trim() || undefined,
        }),
      ),
    };
    try {
      const created = await createWorkflow(client, company, graph);
      onCreated(created);
      onOpenChange(false);
    } catch (e) {
      setError(e instanceof Error ? e.message : "could not create the workflow");
    } finally {
      setSubmitting(false);
    }
  }

  return (
    <Dialog open={open} onOpenChange={(o) => !submitting && onOpenChange(o)}>
      <DialogContent className="max-h-[85vh] overflow-y-auto sm:max-w-2xl">
        <DialogHeader>
          <DialogTitle>New workflow</DialogTitle>
          <DialogDescription>
            Define the graph by hand — nodes, then how they connect.
          </DialogDescription>
        </DialogHeader>

        <div className="grid gap-3 sm:grid-cols-2">
          <div className="grid gap-2">
            <Label htmlFor={`${formId}-id`}>Id</Label>
            <Input
              id={`${formId}-id`}
              value={id}
              onChange={(e) => setId(e.target.value)}
              placeholder="e.g. campaign_pipeline"
            />
          </div>
          <div className="grid gap-2">
            <Label htmlFor={`${formId}-name`}>Name</Label>
            <Input
              id={`${formId}-name`}
              value={name}
              onChange={(e) => setName(e.target.value)}
              placeholder="e.g. Campaign pipeline"
            />
          </div>
        </div>
        <div className="grid gap-2">
          <Label htmlFor={`${formId}-desc`}>Description</Label>
          <Textarea
            id={`${formId}-desc`}
            rows={2}
            value={description}
            onChange={(e) => setDescription(e.target.value)}
            placeholder="What does this workflow do?"
          />
        </div>

        <div className="space-y-2">
          <div className="flex items-center justify-between">
            <Label>Nodes</Label>
            <Button type="button" variant="outline" size="sm" onClick={addNode}>
              <Plus className="size-3.5" /> Add node
            </Button>
          </div>
          <div className="space-y-2">
            {nodes.map((n) => (
              <NodeRow
                key={n.key}
                node={n}
                roster={roster}
                onChange={(fields) => updateNode(n.key, fields)}
                onRemove={() => removeNode(n.key)}
              />
            ))}
            {nodes.length === 0 && (
              <p className="rounded-lg border border-dashed p-3 text-center text-xs text-muted-foreground">
                No nodes yet.
              </p>
            )}
          </div>
        </div>

        <div className="space-y-2">
          <div className="flex items-center justify-between">
            <Label>Edges</Label>
            <Button
              type="button"
              variant="outline"
              size="sm"
              onClick={addEdge}
              disabled={nodes.length < 2}
            >
              <Plus className="size-3.5" /> Add edge
            </Button>
          </div>
          <div className="space-y-2">
            {edges.map((e) => (
              <EdgeRow
                key={e.key}
                edge={e}
                nodeIds={nodes.map((n) => n.id.trim()).filter(Boolean)}
                onChange={(fields) => updateEdge(e.key, fields)}
                onRemove={() => removeEdge(e.key)}
              />
            ))}
            {edges.length === 0 && (
              <p className="rounded-lg border border-dashed p-3 text-center text-xs text-muted-foreground">
                No edges yet — nodes won&apos;t be connected.
              </p>
            )}
          </div>
        </div>

        {error && (
          <Alert variant="destructive">
            <AlertDescription>{error}</AlertDescription>
          </Alert>
        )}

        <DialogFooter>
          <Button variant="ghost" onClick={() => onOpenChange(false)} disabled={submitting}>
            Cancel
          </Button>
          <Button onClick={() => void submit()} disabled={submitting}>
            {submitting ? "Creating…" : "Create workflow"}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}

function NodeRow({
  node,
  roster,
  onChange,
  onRemove,
}: {
  node: DraftNode;
  roster: TeamMemberDto[];
  onChange: (fields: Partial<DraftNode>) => void;
  onRemove: () => void;
}) {
  return (
    <div className="grid gap-2 rounded-lg border p-2 sm:grid-cols-[1fr_1fr_1.4fr_auto] sm:items-start">
      <div className="grid gap-1">
        <Input
          value={node.id}
          onChange={(e) => onChange({ id: e.target.value })}
          placeholder="node id"
          aria-label="Node id"
        />
        <Select value={node.kind} onValueChange={(v) => onChange({ kind: v ?? "" })}>
          <SelectTrigger className="h-8" aria-label="Node kind">
            <SelectValue />
          </SelectTrigger>
          <SelectContent>
            {CREATABLE_NODE_KINDS.map((k) => (
              <SelectItem key={k.value} value={k.value}>
                {k.label}
              </SelectItem>
            ))}
          </SelectContent>
        </Select>
      </div>
      <div className="grid gap-1">
        <Input
          value={node.name}
          onChange={(e) => onChange({ name: e.target.value })}
          placeholder="display name"
          aria-label="Node name"
        />
        {node.kind === "agent" &&
          (roster.length > 0 ? (
            <Select value={node.agent} onValueChange={(v) => onChange({ agent: v ?? "" })}>
              <SelectTrigger className="h-8" aria-label="Teammate">
                <SelectValue placeholder="Pick a teammate" />
              </SelectTrigger>
              <SelectContent>
                {roster.map((m) => (
                  <SelectItem key={m.id} value={m.id}>
                    {m.name ?? m.role}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
          ) : (
            <Input
              value={node.agent}
              onChange={(e) => onChange({ agent: e.target.value })}
              placeholder="teammate id"
              aria-label="Teammate id"
            />
          ))}
      </div>
      <Input
        value={node.summary}
        onChange={(e) => onChange({ summary: e.target.value })}
        placeholder="summary (optional)"
        aria-label="Node summary"
      />
      <Button
        type="button"
        variant="ghost"
        size="icon"
        onClick={onRemove}
        aria-label="Remove node"
        className="justify-self-end"
      >
        <Trash2 className="size-4" />
      </Button>
    </div>
  );
}

function EdgeRow({
  edge,
  nodeIds,
  onChange,
  onRemove,
}: {
  edge: DraftEdge;
  nodeIds: string[];
  onChange: (fields: Partial<DraftEdge>) => void;
  onRemove: () => void;
}) {
  return (
    <div className="grid grid-cols-[1fr_auto_1fr_1fr_auto] items-center gap-2 rounded-lg border p-2">
      <Select value={edge.from} onValueChange={(v) => onChange({ from: v ?? "" })}>
        <SelectTrigger className="h-8" aria-label="Edge from">
          <SelectValue placeholder="from" />
        </SelectTrigger>
        <SelectContent>
          {nodeIds.map((nid) => (
            <SelectItem key={nid} value={nid}>
              {nid}
            </SelectItem>
          ))}
        </SelectContent>
      </Select>
      <span className="text-xs text-muted-foreground">→</span>
      <Select value={edge.to} onValueChange={(v) => onChange({ to: v ?? "" })}>
        <SelectTrigger className="h-8" aria-label="Edge to">
          <SelectValue placeholder="to" />
        </SelectTrigger>
        <SelectContent>
          {nodeIds.map((nid) => (
            <SelectItem key={nid} value={nid}>
              {nid}
            </SelectItem>
          ))}
        </SelectContent>
      </Select>
      <Input
        value={edge.label}
        onChange={(e) => onChange({ label: e.target.value })}
        placeholder="label (optional)"
        aria-label="Edge label"
      />
      <Button
        type="button"
        variant="ghost"
        size="icon"
        onClick={onRemove}
        aria-label="Remove edge"
      >
        <Trash2 className="size-4" />
      </Button>
    </div>
  );
}
