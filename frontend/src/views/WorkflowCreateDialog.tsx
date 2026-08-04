// The workflow creator (issue #69): a plain form editor — not a drag canvas —
// that builds a `WorkflowGraph` and posts it via `createWorkflow`. Node kinds
// are restricted to the ones the engine actually executes today
// (`CREATABLE_NODE_KINDS`); `tool_call`/`http_request` stay off the palette
// until they're wired (see `src/workflows/caps.rs`).
//
// It is also the EDITOR (issue #259): pass a `workflow` and the same form
// hydrates from that saved graph and saves through `updateWorkflow`, carrying
// the graph's `version` as the optimistic-concurrency token. One component
// rather than two because an edit is the same form with the same rules — a
// second one would drift the moment either side grew a field.

import { useEffect, useId, useState } from "react";
import { Plus, Trash2 } from "lucide-react";

import {
  CREATABLE_NODE_KINDS,
  DESTINATION_KINDS,
  createWorkflow,
  updateWorkflow,
  type WorkflowDestination,
  type WorkflowEdge,
  type WorkflowGraph,
  type WorkflowNode,
} from "@/api/workflows";
import type { OpenCompanyClient } from "@/api/client";
import { ApiError } from "@/api/types";
import { CronPreviewLine } from "@/views/CronPreviewLine";
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
  /** The trigger's cron expression (issue #169). Empty means "no schedule";
   * only ever set on the trigger node, which is where the host allows it. */
  schedule: string;
  /** Output nodes only. `""` means "don't route this anywhere" — the pre-#170
   * behaviour, where the report only shows in the run drawer. */
  destinationKind: "" | WorkflowDestination["kind"];
  /** The address (`email`) or channel id (`channel`). Unused for `owner`. */
  destinationTarget: string;
  /**
   * Fields the form has no control for, carried through an edit **verbatim**
   * (issue #259).
   *
   * An overlay graph can carry them — `POST`/`PUT` accept them and the
   * orchestrator's own `create_workflow` tool writes them — so a graph authored
   * outside this dialog can reach it. Rebuilding the node from the visible
   * controls alone would then quietly delete a retry policy or an approval gate
   * on the first save, which is a worse bug than the write-once one this fixes.
   *
   * They ride on the ROW, not on the node id, so they follow the row when its
   * id is edited. `config` is the exception: it is kind-specific, so
   * {@link changeKind} drops it along with every other kind-conditional field.
   */
  config?: unknown;
  onError?: string;
  retry?: WorkflowNode["retry"];
  requiresApproval?: boolean;
}

/** "No schedule" — the workflow runs only when something starts it. A sentinel
 * rather than `""` because a select option with an empty value is ambiguous. */
const NO_SCHEDULE = "none";
/** "Type your own cron." Neither sentinel is a valid 5-field cron, so neither
 * can collide with a preset or a custom value. */
const CUSTOM_SCHEDULE = "custom";

/** The friendly schedule choices offered on the trigger row. Each preset emits
 * a real 5-field cron — the host only ever stores and matches cron, so the
 * friendliness lives here rather than in a second wire format. Times are UTC,
 * which the hint under the field says out loud. */
const SCHEDULE_PRESETS = [
  { value: NO_SCHEDULE, label: "No schedule (run manually)" },
  { value: "0 * * * *", label: "Hourly — on the hour" },
  { value: "0 9 * * *", label: "Daily — 09:00 UTC" },
  { value: "0 9 * * MON", label: "Weekly — Monday 09:00 UTC" },
] as const;

/** Whether `cron` is one of the presets (so the Select shows it directly rather
 * than dropping into the custom input). An empty schedule is "none". */
function isPresetSchedule(cron: string): boolean {
  if (cron === "") return true;
  return SCHEDULE_PRESETS.some((p) => p.value === cron);
}

/** A cheap 5-field shape check, mirroring the host's `CronExpr::parse` arity
 * rule so the obvious mistake ("hourly", "every day") is caught before a round
 * trip. Real validation — ranges, names, steps — is the server's 400. */
function looksLikeCron(cron: string): boolean {
  return cron.trim().split(/\s+/).length === 5;
}

/** What is wrong with `schedule`, or `null` when it is postable.
 *
 * One field, one message, no node context — so the same rule can answer both
 * callers: `validate()` at submit (which prefixes the node it belongs to) and
 * the field's own blur handler (which shows it under the input). An empty
 * schedule is fine: "no schedule" is a real choice.
 */
function scheduleProblem(schedule: string): string | null {
  if (!schedule.trim()) return null;
  if (!looksLikeCron(schedule)) {
    return "A schedule is a 5-field cron, e.g. `0 9 * * MON` (minute hour day month weekday).";
  }
  return null;
}

/** What is wrong with an output node's `destination.target` for `kind`, or
 * `null` when it is postable. Mirrors the host's per-kind target contract in
 * `src/company/workflow_file.rs`; `owner` and "no destination" carry no target
 * and so have nothing to check.
 *
 * Same two-caller contract as {@link scheduleProblem}.
 *
 * Issue #260: each message ends with the SAME fix instruction the host's
 * rejection ends with, and echoes the offending target the same way, so an
 * author who trips the pre-flight and an author who trips the 400 are told the
 * same thing. `destination_messages_match_the_console` in
 * `src/company/workflow_file.rs` fails if either side is reworded alone.
 */
function destinationTargetProblem(
  kind: DraftNode["destinationKind"],
  target: string,
): string | null {
  const value = target.trim();
  if (kind === "email" && !value.includes("@")) {
    return `\`${value}\` is not an email address — give the recipient's full address.`;
  }
  if (kind === "channel" && !value) {
    return "A channel destination needs a channel id — name the channel to post the report to.";
  }
  return null;
}

/** How a validation message names a node.
 *
 * Issue #260: the dialog reported `Node \`2\`` — the id, which on a row the
 * author never renamed is whatever the form put there — while the author had
 * typed a name. Prefer the name they chose; fall back to the id, and to a
 * position-free phrase when the row is still blank (which the "needs an id"
 * check above will have already reported).
 */
function nodeLabel(node: DraftNode): string {
  return node.name.trim() || node.id.trim() || "this node";
}

interface DraftEdge {
  key: string;
  from: string;
  to: string;
  label: string;
}

/** The node fields that validate on blur (issue #261) — the ones with a real
 * contract, which are the ones authors get wrong. */
type ValidatedField = "schedule" | "destinationTarget";

/** The key a field's error is filed under.
 *
 * Deliberately `node.key`, the stable row key, and NOT `node.id`: the id is a
 * text field the author edits, so keying on it would strand every error the
 * moment they renamed a node — the error would still render, attached to
 * nothing that can clear it.
 */
function errorKey(nodeKey: string, field: ValidatedField): string {
  return `${nodeKey}:${field}`;
}

/** The "no destination" option's value. A Select item cannot carry an empty
 * string, so the sentinel stands in for `destinationKind: ""`. */
const NO_DESTINATION = "__none__";

let seq = 0;
function nextKey(): string {
  seq += 1;
  return `row-${seq}`;
}

/** The field updates for changing a node's kind: the new kind, plus a reset of
 * every field only the OLD kind's controls could edit.
 *
 * The rule is "draft state matches the visible controls", and it covers EVERY
 * kind-conditional field — `agent` (agent nodes), `schedule` (trigger nodes),
 * and the destination pair (output nodes). Clearing beats tolerating a stale
 * value: `submit()` already drops fields that don't match the kind, but a stale
 * `destinationKind` also had to pass validation, and there was no control left
 * on screen to fix it with. `agent` and `schedule` never trapped the form that
 * way; they are cleared so there is one rule here rather than three, and so
 * that switching a node's kind and back doesn't silently resurrect a value the
 * author can no longer see.
 *
 * Anything added to `DraftNode` behind a `node.kind === …` control belongs in
 * this reset.
 */
function changeKind(kind: string): Partial<DraftNode> {
  return {
    kind,
    agent: "",
    schedule: "",
    destinationKind: "",
    destinationTarget: "",
    // Kind-specific by definition (a `switch`'s cases, a `sub_workflow`'s
    // target), so it means nothing on the new kind — and unlike the fields
    // above there is no control that could ever show what was dropped. The
    // kind-agnostic policies (`onError`, `retry`, `requiresApproval`) are kept.
    config: undefined,
  };
}

/** A blank node row, so every construction site stays in step as the shape grows. */
function blankNode(fields: Partial<DraftNode> = {}): DraftNode {
  return {
    key: nextKey(),
    id: "",
    kind: "agent",
    name: "",
    summary: "",
    agent: "",
    schedule: "",
    destinationKind: "",
    destinationTarget: "",
    ...fields,
  };
}

function starterNodes(): DraftNode[] {
  return [blankNode({ id: "start", kind: "trigger", name: "Start" })];
}

/** The saved graph's nodes as draft rows (issue #259).
 *
 * Every row goes through {@link blankNode}, so each gets a **fresh** `key` from
 * `nextKey()`. Reusing the saved node ids as keys would be the obvious shortcut
 * and a real bug: `fieldErrors` is keyed on `key`, so two graphs that share a
 * node id (`start` is the starter row's id, so most of them do) would share an
 * error map, and a complaint raised on one graph would render on the next.
 */
function draftNodes(graph: WorkflowGraph): DraftNode[] {
  return graph.nodes.map((n) =>
    blankNode({
      id: n.id,
      kind: n.kind,
      name: n.name,
      summary: n.summary ?? "",
      agent: n.agent ?? "",
      schedule: n.schedule ?? "",
      destinationKind: n.destination?.kind ?? "",
      destinationTarget: n.destination?.target ?? "",
      config: n.config,
      onError: n.onError,
      retry: n.retry,
      requiresApproval: n.requiresApproval,
    }),
  );
}

/** The saved graph's edges as draft rows, on the same fresh-key rule. */
function draftEdges(graph: WorkflowGraph): DraftEdge[] {
  return graph.edges.map((e) => ({
    key: nextKey(),
    from: e.from,
    to: e.to,
    label: e.label ?? "",
  }));
}

/** A safe on-disk id: only letters, digits, `_`, and `-` — a subset of what the
 * host's `safe_wid` accepts (any single path component), chosen to keep ids
 * simple and unambiguous without a round-trip to the server first. */
function isSafeId(id: string): boolean {
  return /^[A-Za-z0-9_-]+$/.test(id);
}

export function WorkflowCreateDialog({
  client,
  company,
  open,
  onOpenChange,
  onCreated,
  workflow = null,
  onSaved,
  onConflict,
}: {
  client: OpenCompanyClient;
  company: string | null;
  open: boolean;
  onOpenChange: (open: boolean) => void;
  /** Called with the stored graph after a create. Create mode only. */
  onCreated?: (graph: WorkflowGraph) => void;
  /**
   * The saved graph to edit (issue #259). `null` is create mode. Pass the graph
   * straight from `getWorkflow` — its `version` is what makes the save
   * conditional, so a copy without one silently loses the guard.
   */
  workflow?: WorkflowGraph | null;
  /** Called with the stored graph, and its FRESH version, after an edit. */
  onSaved?: (graph: WorkflowGraph) => void;
  /**
   * The host's message when it refused the save with a `409` — the graph moved
   * under this edit, or the new display name is taken. The dialog stays open
   * with the same message inline (so the author keeps what they typed); this
   * hands it to the view, whose persistent banner carries the way out (Reload).
   */
  onConflict?: (message: string) => void;
}) {
  const editing = workflow !== null;
  const [id, setId] = useState("");
  const [name, setName] = useState("");
  const [description, setDescription] = useState("");
  const [nodes, setNodes] = useState<DraftNode[]>(starterNodes());
  const [edges, setEdges] = useState<DraftEdge[]>([]);
  const [roster, setRoster] = useState<TeamMemberDto[]>([]);
  const [submitting, setSubmitting] = useState(false);
  const [error, setError] = useState<string | null>(null);
  /** Per-field problems raised on blur (issue #261), keyed by
   * {@link errorKey}. Separate from `error`, the submit-time banner: this one
   * is inline, scoped to the control that caused it, and never blocks Save on
   * its own — `validate()` remains the gate. */
  const [fieldErrors, setFieldErrors] = useState<Record<string, string>>({});
  const formId = useId();

  // Reload the roster (for the agent-node picker) and reset the draft each
  // time the dialog opens, so a prior attempt never leaks into the next one.
  //
  // Edit mode hydrates HERE rather than anywhere else on purpose (issue #259):
  // this is the one place `fieldErrors` is cleared, so a draft populated by any
  // other path would carry the previous graph's errors — attached to rows that
  // no longer exist and pointing at fields nobody can see.
  //
  // `workflow` is a dependency, so the conflict banner's Reload re-hydrates an
  // open dialog with the fresh graph and its fresh token. That discards what
  // was typed, which is the honest outcome: Reload means "show me the latest",
  // and keeping the edit would keep the stale token with it.
  useEffect(() => {
    if (!open) return;
    setId(workflow?.id ?? "");
    setName(workflow?.name ?? "");
    setDescription(workflow?.description ?? "");
    setNodes(workflow ? draftNodes(workflow) : starterNodes());
    setEdges(workflow ? draftEdges(workflow) : []);
    setError(null);
    setFieldErrors({});
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
  }, [open, client, company, workflow]);

  function addNode() {
    setNodes((rows) => [...rows, blankNode()]);
  }

  function updateNode(key: string, fields: Partial<DraftNode>) {
    setNodes((rows) => rows.map((r) => (r.key === key ? { ...r, ...fields } : r)));
    // Clear whatever the edit invalidated. This MUST stay in step with
    // `changeKind`: that reset exists so the draft never holds a value whose
    // control is off screen, and an error is a value too — leaving one behind
    // would show a complaint about a field the author can no longer see, let
    // alone fix. Same reasoning for `destinationKind`, which swaps which target
    // contract (address vs channel id) applies.
    setFieldErrors((prev) => {
      const stale: ValidatedField[] = [];
      if ("kind" in fields) stale.push("schedule", "destinationTarget");
      if ("destinationKind" in fields) stale.push("destinationTarget");
      // Typing in a field clears its own error: the author is already fixing
      // it, and re-checking mid-word would fail on every prefix of a correct
      // answer (`n`, `no`, `nop` on the way to an address).
      if ("schedule" in fields) stale.push("schedule");
      if ("destinationTarget" in fields) stale.push("destinationTarget");

      const next = { ...prev };
      let changed = false;
      for (const field of stale) {
        const k = errorKey(key, field);
        if (k in next) {
          delete next[k];
          changed = true;
        }
      }
      return changed ? next : prev;
    });
  }

  /** Checks one field's own rule, on blur. Returns nothing — the result lands
   * in `fieldErrors` — so the caller stays a one-liner on the control.
   *
   * An EMPTY field is never flagged here. "You haven't filled this in yet" is
   * true of every field an author tabs past on the way to somewhere else;
   * saying so is nagging, not feedback. Emptiness stays `validate()`'s business
   * at submit, where it is actually a problem. */
  function validateField(nodeKey: string, field: ValidatedField, value: string) {
    if (!value.trim()) return;
    const node = nodes.find((n) => n.key === nodeKey);
    if (!node) return;
    const problem =
      field === "schedule"
        ? scheduleProblem(value)
        : destinationTargetProblem(node.destinationKind, value);
    if (problem) {
      setFieldErrors((prev) => ({ ...prev, [errorKey(nodeKey, field)]: problem }));
    }
  }

  function removeNode(key: string) {
    const removed = nodes.find((n) => n.key === key);
    setNodes((rows) => rows.filter((r) => r.key !== key));
    // The row is gone, so its errors have nothing left to point at.
    setFieldErrors((prev) => {
      const next = Object.fromEntries(
        Object.entries(prev).filter(([k]) => !k.startsWith(`${key}:`)),
      );
      return Object.keys(next).length === Object.keys(prev).length ? prev : next;
    });
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
      if (!n.name.trim()) return `Node \`${nodeLabel(n)}\` needs a name.`;
      if (n.kind === "agent" && !n.agent.trim()) {
        return `Node \`${nodeLabel(n)}\` is an agent node — pick who does it.`;
      }
      // Only fires for a node that IS a trigger, so this is a check on visible
      // state, never an off-kind trap.
      if (n.kind === "trigger") {
        const problem = scheduleProblem(n.schedule);
        if (problem) return `Node \`${nodeLabel(n)}\`: ${problem}`;
      }
      // Mirrors the host's `destination` target rules so a wrong target is
      // caught here rather than after a round trip. There is deliberately NO
      // "destination on a non-output node" check: `changeKind` makes that state
      // unreachable, and re-adding the check would only recreate the trap of an
      // error the author has no visible control to clear.
      const destinationProblem = destinationTargetProblem(
        n.destinationKind,
        n.destinationTarget,
      );
      if (destinationProblem) return `Node \`${nodeLabel(n)}\`: ${destinationProblem}`;
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
          // The host rejects a schedule on any non-trigger node, so only the
          // trigger's value is ever sent.
          schedule:
            n.kind === "trigger" && n.schedule.trim() ? n.schedule.trim() : undefined,
          // Only output nodes route a report, and `owner` resolves server-side
          // so it must carry no target — the host rejects one.
          destination:
            n.kind === "output" && n.destinationKind
              ? {
                  kind: n.destinationKind,
                  target:
                    n.destinationKind === "owner"
                      ? undefined
                      : n.destinationTarget.trim() || undefined,
                }
              : undefined,
          // Whatever the form has no control for, straight back out again — an
          // edit must not delete what it cannot show. Always `undefined` on a
          // create, and `undefined` is omitted from the JSON body.
          config: n.config,
          onError: n.onError,
          retry: n.retry,
          requiresApproval: n.requiresApproval,
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
      if (workflow) {
        // The id keys the saved graph, the schedule and the run history, so it
        // is the graph's own id that is sent, not the (read-only) field —
        // there is no path here that renames anything, and the host answers
        // 400 if one ever appeared. `version` makes the write conditional: it
        // means "save over the graph I was looking at", not "over whatever is
        // there now". The response carries a fresh token, so a second save
        // needs no intervening read.
        const saved = await updateWorkflow(
          client,
          company,
          workflow.id,
          graph,
          workflow.version,
        );
        onSaved?.(saved);
      } else {
        const created = await createWorkflow(client, company, graph);
        onCreated?.(created);
      }
      onOpenChange(false);
    } catch (e) {
      setError(
        e instanceof Error
          ? e.message
          : workflow
            ? "could not save the workflow"
            : "could not create the workflow",
      );
      // A refused write is the one failure the operator can act on, and the
      // action (reload, or pick another name) happens out in the view — so it
      // is raised there too, where the banner persists past this dialog. The
      // dialog stays open with the same message so the edit is not thrown away.
      if (workflow && e instanceof ApiError && e.status === 409) {
        onConflict?.(e.message);
      }
    } finally {
      setSubmitting(false);
    }
  }

  return (
    <Dialog open={open} onOpenChange={(o) => !submitting && onOpenChange(o)}>
      <DialogContent className="max-h-[85vh] overflow-y-auto sm:max-w-2xl">
        <DialogHeader>
          <DialogTitle>{editing ? "Edit workflow" : "New workflow"}</DialogTitle>
          <DialogDescription>
            {editing
              ? "Change the nodes, how they connect, or when it runs. Saving replaces the whole graph."
              : "Define the graph by hand — nodes, then how they connect."}
          </DialogDescription>
        </DialogHeader>

        <div className="grid gap-3 sm:grid-cols-2">
          <div className="grid gap-2">
            <Label htmlFor={`${formId}-id`}>Id</Label>
            {/* Read-only in edit mode, not merely rejected on save: the id keys
                the saved graph, the scheduler and every past run, so the host
                answers 400 to a rename. Letting an author type a new one and
                then refusing it would be a trap. */}
            <Input
              id={`${formId}-id`}
              value={id}
              onChange={(e) => setId(e.target.value)}
              readOnly={editing}
              aria-readonly={editing || undefined}
              className={editing ? "text-muted-foreground" : undefined}
              placeholder="e.g. campaign_pipeline"
            />
            {editing && (
              <p className="text-[11px] leading-snug text-muted-foreground">
                A workflow&apos;s id can&apos;t change. It keys the saved graph, its
                schedule and its run history.
              </p>
            )}
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
                client={client}
                company={company}
                roster={roster}
                errors={{
                  schedule: fieldErrors[errorKey(n.key, "schedule")],
                  destinationTarget: fieldErrors[errorKey(n.key, "destinationTarget")],
                }}
                onValidateField={(field, value) => validateField(n.key, field, value)}
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
          <Button
            onClick={() => void submit()}
            disabled={submitting}
            data-testid="workflow-dialog-submit"
          >
            {editing
              ? submitting
                ? "Saving…"
                : "Save changes"
              : submitting
                ? "Creating…"
                : "Create workflow"}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}

/** A one-line problem shown under the control that caused it (issue #261). */
function FieldError({ id, message }: { id: string; message?: string }) {
  if (!message) return null;
  return (
    <p id={id} className="text-[11px] leading-snug text-destructive">
      {message}
    </p>
  );
}

function NodeRow({
  node,
  client,
  company,
  roster,
  errors,
  onValidateField,
  onChange,
  onRemove,
}: {
  node: DraftNode;
  /** Threaded through solely so the trigger row's schedule field can ask the
   * host what its cron means (issue #262). */
  client: OpenCompanyClient;
  company: string | null;
  roster: TeamMemberDto[];
  /** Blur-time problems for this row's validated fields, if any. */
  errors: Partial<Record<ValidatedField, string>>;
  onValidateField: (field: ValidatedField, value: string) => void;
  onChange: (fields: Partial<DraftNode>) => void;
  onRemove: () => void;
}) {
  const rowId = useId();
  const targetErrorId = `${rowId}-target-error`;
  return (
    <div className="grid gap-2 rounded-lg border p-2 sm:grid-cols-[1fr_1fr_1.4fr_auto] sm:items-start">
      <div className="grid gap-1">
        <Input
          value={node.id}
          onChange={(e) => onChange({ id: e.target.value })}
          placeholder="node id"
          aria-label="Node id"
        />
        {/* Changing the kind clears every kind-conditional field, so the draft
            never holds a value whose control is no longer on screen. Without
            this, picking a destination and then changing the kind left the row
            un-submittable with nothing visible to clear. */}
        <Select value={node.kind} onValueChange={(v) => onChange(changeKind(v ?? ""))}>
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
      <div className="grid gap-1">
        <Input
          value={node.summary}
          onChange={(e) => onChange({ summary: e.target.value })}
          placeholder="summary (optional)"
          aria-label="Node summary"
        />
        {node.kind === "trigger" && (
          <ScheduleField
            client={client}
            company={company}
            schedule={node.schedule}
            error={errors.schedule}
            onChange={(schedule) => onChange({ schedule })}
            onBlurValidate={(value) => onValidateField("schedule", value)}
          />
        )}
        {/* Only an output node reports back, so only it can route that report
            somewhere. "Nowhere" stays the default: the result still shows in the
            run drawer, which is all an output node did before. */}
        {node.kind === "output" && (
          <>
            <Select
              value={node.destinationKind || NO_DESTINATION}
              onValueChange={(v) =>
                onChange({
                  destinationKind:
                    !v || v === NO_DESTINATION
                      ? ""
                      : (v as WorkflowDestination["kind"]),
                  // Switching to `owner` (or to no destination) clears the
                  // target — the host rejects an `owner` that carries one.
                  ...(v === "owner" || !v || v === NO_DESTINATION
                    ? { destinationTarget: "" }
                    : {}),
                })
              }
            >
              <SelectTrigger className="h-8" aria-label="Send report to">
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                <SelectItem value={NO_DESTINATION}>
                  Send report to… nowhere (run result only)
                </SelectItem>
                {DESTINATION_KINDS.map((d) => (
                  <SelectItem key={d.value} value={d.value}>
                    {d.label}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
            {(node.destinationKind === "email" || node.destinationKind === "channel") && (
              <>
                <Input
                  value={node.destinationTarget}
                  onChange={(e) => onChange({ destinationTarget: e.target.value })}
                  onBlur={(e) => onValidateField("destinationTarget", e.target.value)}
                  aria-invalid={Boolean(errors.destinationTarget)}
                  aria-describedby={errors.destinationTarget ? targetErrorId : undefined}
                  placeholder={
                    node.destinationKind === "email" ? "recipient@example.com" : "channel id"
                  }
                  aria-label={
                    node.destinationKind === "email" ? "Recipient address" : "Channel id"
                  }
                />
                <FieldError id={targetErrorId} message={errors.destinationTarget} />
              </>
            )}
            {node.destinationKind === "email" && (
              <p className="text-[11px] leading-snug text-muted-foreground">
                Only sends if this company grants email and the recipient has
                already written in.
              </p>
            )}
          </>
        )}
      </div>
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

/** The trigger row's schedule control (issue #169): a preset picker that emits
 * real cron strings, plus a Custom escape hatch for anything the presets don't
 * cover. Rendered only on the trigger node, mirroring how the teammate picker
 * appears only on agent nodes. */
function ScheduleField({
  client,
  company,
  schedule,
  error,
  onChange,
  onBlurValidate,
}: {
  client: OpenCompanyClient;
  company: string | null;
  schedule: string;
  /** The blur-time cron problem for this field, when there is one. */
  error?: string;
  onChange: (schedule: string) => void;
  onBlurValidate: (value: string) => void;
}) {
  const fieldId = useId();
  const errorId = `${fieldId}-error`;
  // A non-empty value that isn't a preset means the operator typed their own.
  const custom = schedule !== "" && !isPresetSchedule(schedule);
  // Track "Custom is selected but nothing typed yet" so the input stays open.
  const [customOpen, setCustomOpen] = useState(custom);
  const showCustom = custom || customOpen;

  return (
    <div className="grid gap-1">
      <Select
        value={showCustom ? CUSTOM_SCHEDULE : schedule || NO_SCHEDULE}
        onValueChange={(v) => {
          if (v === CUSTOM_SCHEDULE) {
            setCustomOpen(true);
            return;
          }
          setCustomOpen(false);
          onChange(v === NO_SCHEDULE || !v ? "" : v);
        }}
      >
        <SelectTrigger className="h-8" aria-label="Schedule">
          <SelectValue placeholder="No schedule (run manually)" />
        </SelectTrigger>
        <SelectContent>
          {SCHEDULE_PRESETS.map((p) => (
            <SelectItem key={p.value} value={p.value}>
              {p.label}
            </SelectItem>
          ))}
          <SelectItem value={CUSTOM_SCHEDULE}>Custom cron…</SelectItem>
        </SelectContent>
      </Select>
      {showCustom && (
        <Input
          className="h-8 font-mono text-xs"
          value={schedule}
          onChange={(e) => onChange(e.target.value)}
          onBlur={(e) => onBlurValidate(e.target.value)}
          aria-invalid={Boolean(error)}
          aria-describedby={error ? errorId : undefined}
          placeholder="0 9 * * MON"
          aria-label="Custom cron schedule"
        />
      )}
      <FieldError id={errorId} message={error} />
      {(showCustom || schedule) && (
        <p className="text-[10px] text-muted-foreground">
          5-field cron. Times are UTC.
        </p>
      )}
      {/* The hint above says the contract; this says what THIS expression means
          under it (issue #262) — including for a preset, since "Daily — 09:00
          UTC" is only obviously wrong once you see it land at 14:30 your time.
          Gated on the same 5-field shape check the pre-flight uses, so nothing
          goes on the wire until there is a whole expression to read. */}
      {looksLikeCron(schedule) && (
        <CronPreviewLine
          client={client}
          company={company}
          schedule={schedule}
          suppressError={Boolean(error)}
        />
      )}
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
