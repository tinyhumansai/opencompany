// The workflow creator (issue #69): a plain form editor — not a drag canvas —
// that builds a `WorkflowGraph` and posts it via `createWorkflow`. Node kinds
// are the ones the engine executes and the console can author from a form
// (`CREATABLE_NODE_KINDS`). The five that need kind-specific config —
// `tool_call`, `http_request`, `switch`, `output_parser`, `sub_workflow` —
// grew their controls in issue #541; each renders `NodeConfigFields`, whose
// spec table (`@/lib/workflow-node-config`) is the single source of the engine
// keys each kind emits.
//
// It is also the EDITOR (issue #259): pass a `workflow` and the same form
// hydrates from that saved graph and saves through `updateWorkflow`, carrying
// the graph's `version` as the optimistic-concurrency token. One component
// rather than two because an edit is the same form with the same rules — a
// second one would drift the moment either side grew a field.

import { useEffect, useId, useRef, useState } from "react";
import { History, Loader2, Plus, RotateCcw, Sparkles, Trash2 } from "lucide-react";

import {
  CREATABLE_NODE_KINDS,
  DESTINATION_KINDS,
  destinationLabel,
  createWorkflow,
  draftWorkflowFromDescription,
  listWiredChannels,
  listWorkflowRevisions,
  listWorkflows,
  restoreWorkflowRevision,
  updateWorkflow,
  type WorkflowDestination,
  type WorkflowEdge,
  type WorkflowGraph,
  type WorkflowNode,
  type WorkflowReadiness,
  type WorkflowRevision,
  type WorkflowSummary,
  type PrefilledDraft,
} from "@/api/workflows";
import { getInferenceStatus, type CognitionPath } from "@/api/inference";
import {
  blankConfigDraft,
  configDraftFrom,
  configDraftProblem,
  configFieldSpecs,
  configFieldProblem,
  configFromDraft,
  hasConfigForm,
} from "@/lib/workflow-node-config";
import { draftBanners } from "@/lib/workflow-draft";
import type { OpenCompanyClient } from "@/api/client";
import { ApiError } from "@/api/types";
import { CronPreviewLine } from "@/views/CronPreviewLine";
import { NodeConfigFields } from "@/views/workflows/NodeConfigFields";
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
   *
   * `config` here carries the raw overlay only for kinds WITHOUT a config form.
   * The five form kinds (issue #541) instead hold their config in
   * {@link configDraft} (the form strings) and {@link configExtra} (keys with
   * no control, preserved verbatim — the same anti-data-loss guard, but per
   * key). `submit()` rebuilds their `config` from those two.
   */
  config?: unknown;
  /** Per-field config strings for a form kind, keyed by engine key (#541).
   * Empty `{}` for kinds without a form. */
  configDraft: Record<string, string>;
  /** A form kind's config keys the form has no control for, kept verbatim so an
   * edit never drops an orchestrator-authored `connection_ref`/`execution`/… */
  configExtra?: Record<string, unknown>;
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
export function destinationTargetProblem(
  kind: DraftNode["destinationKind"],
  target: string,
  wiredChannels: string[],
): string | null {
  const value = target.trim();
  if (kind === "email" && !value.includes("@")) {
    return `\`${value}\` is not an email address — give the recipient's full address.`;
  }
  if (kind === "channel" && !value) {
    return "A channel destination needs a channel id — name the channel to post the report to.";
  }
  // #813: when the host told us which channels it can deliver to, a target that
  // is not one of them is refused when the workflow is saved (#981) and, for a
  // graph saved before a desk went away, fails at delivery (`ChannelNotWired`) —
  // catch it at author time instead. Skipped when the list is empty (host
  // offered none), so a degraded free-text box is never wrongly rejected.
  //
  // #981: the same sentence the host's save-time rejection carries, so an author
  // who trips the pre-flight and an author who trips the 400 are told the same
  // thing. `operator` is no longer in the list the host serves — it never was a
  // delivery target — so this is what now catches it.
  if (
    kind === "channel" &&
    value &&
    wiredChannels.length > 0 &&
    !wiredChannels.includes(value)
  ) {
    return `\`${value}\` is not a workflow delivery channel — this runtime has: ${wiredChannels.join(", ")}.`;
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
 * contract, which are the ones authors get wrong. `config:${key}` covers the
 * kind-specific config fields (issue #541), filed under the field's engine key
 * (e.g. `config:slug`). */
type ValidatedField = "schedule" | "destinationTarget" | `config:${string}`;

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
    // Kind-specific by definition (a `switch`'s branch key, a `sub_workflow`'s
    // target), so it means nothing on the new kind. `config` is the raw overlay
    // for form-less kinds; `configDraft`/`configExtra` are the form kinds'
    // (#541). All three reset to the new kind's blank state — the kind-agnostic
    // policies (`onError`, `retry`, `requiresApproval`) are kept.
    config: undefined,
    configDraft: blankConfigDraft(kind),
    configExtra: undefined,
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
    configDraft: {},
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
  return graph.nodes.map((n) => {
    const common = {
      id: n.id,
      kind: n.kind,
      name: n.name,
      summary: n.summary ?? "",
      agent: n.agent ?? "",
      schedule: n.schedule ?? "",
      destinationKind: (n.destination?.kind ?? "") as DraftNode["destinationKind"],
      destinationTarget: n.destination?.target ?? "",
      onError: n.onError,
      retry: n.retry,
      requiresApproval: n.requiresApproval,
    };
    // A form kind (#541) hydrates its config into per-field strings plus a
    // preserved `extra` bag; a form-less kind keeps the raw overlay in `config`.
    if (hasConfigForm(n.kind)) {
      const { draft, extra } = configDraftFrom(n.kind, n.config);
      return blankNode({ ...common, configDraft: draft, configExtra: extra });
    }
    return blankNode({ ...common, config: n.config, configDraft: {} });
  });
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
  prefilledDraft = null,
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
  /**
   * A copilot-corrected graph to hydrate the form with directly (issue #840,
   * PR-3), skipping the description→draft round trip. Combined with `workflow`
   * (the saved graph the correction targets) this opens the dialog in edit mode
   * showing the corrected nodes/edges, so Save writes a new *version* under the
   * same id. The summary/notes/readiness banners render read-only.
   */
  prefilledDraft?: PrefilledDraft | null;
}) {
  const editing = workflow !== null;
  const [id, setId] = useState("");
  const [name, setName] = useState("");
  const [description, setDescription] = useState("");
  const [nodes, setNodes] = useState<DraftNode[]>(starterNodes());
  const [edges, setEdges] = useState<DraftEdge[]>([]);
  const [roster, setRoster] = useState<TeamMemberDto[]>([]);
  /** The chat channels this company can actually deliver to (#813): the picker
   * options for an output node's `channel` destination. Degrades to a free-text
   * box when the host offers no list, so authoring is never blocked. */
  const [wiredChannels, setWiredChannels] = useState<string[]>([]);
  /** The company's workflows, for the `sub_workflow` config picker (#541). The
   * graph's own id is dropped at render time — a sub-workflow can't call
   * itself. Degrades to a free-text id field when the host offers no list. */
  const [workflows, setWorkflows] = useState<WorkflowSummary[]>([]);
  const [submitting, setSubmitting] = useState(false);
  /** The same "a write is in flight" fact as `submitting`, held where `submit()`
   * can read it SYNCHRONOUSLY (issue #1005). `submitting` is captured from the
   * render that produced the handler, so two calls landing before React commits
   * `setSubmitting(true)` — a double activation, an Enter keypress arriving with
   * a click, any caller added later — would both read `false` and both post. The
   * state stays because the render needs it; the ref is what actually guards. */
  const submittingRef = useRef(false);
  const [error, setError] = useState<string | null>(null);
  /** The submit-time error banner, so a failed submit can scroll it into view
   * and focus it rather than leave the message off-screen (#813 defect 6). */
  const errorRef = useRef<HTMLDivElement>(null);
  /** Per-field problems raised on blur (issue #261), keyed by
   * {@link errorKey}. Separate from `error`, the submit-time banner: this one
   * is inline, scoped to the control that caused it, and never blocks Save on
   * its own — `validate()` remains the gate. */
  const [fieldErrors, setFieldErrors] = useState<Record<string, string>>({});
  // Issue #274: the edit history panel (edit mode only). It fetches lazily — the
  // first time an operator expands it — so opening the dialog to make an edit
  // costs no extra request unless they actually want to look back.
  const [historyOpen, setHistoryOpen] = useState(false);
  const [revisions, setRevisions] = useState<WorkflowRevision[]>([]);
  const [revisionsLoaded, setRevisionsLoaded] = useState(false);
  const [revisionsLoading, setRevisionsLoading] = useState(false);
  const [revisionsError, setRevisionsError] = useState<string | null>(null);
  /** The revision currently being restored, so its row can show a spinner and
   * every Restore button disables while one restore is in flight. */
  const [restoringId, setRestoringId] = useState<string | null>(null);
  // Issue #753: the create-time copilot. Create mode only — an edit already has
  // a graph to change. `cognition` gates the composer the same way CopilotPanel
  // does: on the offline `echo` brain there is no model to draft with, so Draft
  // is disabled rather than failing on click. `null` until the check settles
  // (and on a host without the route), which leaves it enabled — refusing to
  // draft because we could not confirm would break it on hosts where it works.
  const [copilotPrompt, setCopilotPrompt] = useState("");
  const [drafting, setDrafting] = useState(false);
  const [draftError, setDraftError] = useState<string | null>(null);
  const [draftSummary, setDraftSummary] = useState<string | null>(null);
  const [draftReason, setDraftReason] = useState<string | null>(null);
  // Host corrections the copilot made to the draft (issue #813) — e.g. a
  // name/role→id rewrite — shown under the summary so the author sees WHY the
  // hydrated graph differs from a literal reading of their request.
  const [draftNotes, setDraftNotes] = useState<string[]>([]);
  // Issue #840 (PR-3): the static readiness advisories over a copilot-corrected
  // graph handed in via `prefilledDraft`. Read-only — it never blocks Save.
  const [readiness, setReadiness] = useState<WorkflowReadiness | null>(null);
  const [cognition, setCognition] = useState<CognitionPath | null>(null);
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
    // Issue #274: a fresh open (or a re-hydrate after a restore) must not carry
    // the previous graph's history. It re-loads on the next expand, and against
    // the freshly-restored body's version token.
    setHistoryOpen(false);
    setRevisions([]);
    setRevisionsLoaded(false);
    setRevisionsError(null);
    setRestoringId(null);
    // Issue #753: a fresh open never carries a prior draft's prompt or result.
    setCopilotPrompt("");
    setDraftError(null);
    setDraftSummary(null);
    setDraftReason(null);
    setDraftNotes([]);
    // Issue #840 (PR-3): a copilot-corrected graph handed in hydrates the form
    // directly — nodes/edges/name from the correction, not the description round
    // trip — while `workflow` above still supplies the id + version token the
    // conditional Save writes under. Its banners (summary/notes/readiness) render
    // read-only below. Absent → a normal open, and the readiness banner clears.
    //
    // `g.id` comes from `workflow.id`, host-pinned server-side (the fix route
    // never lets the copilot's own id vote — see `FixTarget`), so it is not
    // attacker-influenceable in the current call path. `workflow?.id` is
    // preferred anyway: it is the id the conditional Save on this dialog
    // actually writes under, so hydrating from anything else would only ever
    // be a latent bug if that invariant ever drifted.
    if (prefilledDraft) {
      const g = prefilledDraft.workflow;
      setId(workflow?.id ?? g.id);
      setName(g.name.trim());
      setDescription(g.description ?? "");
      setNodes(draftNodes(g));
      setEdges(draftEdges(g));
      setDraftSummary(prefilledDraft.summary ?? null);
      setDraftNotes((prefilledDraft.notes ?? []).filter((n) => n.trim()));
      setReadiness(prefilledDraft.readiness ?? null);
    } else {
      setReadiness(null);
    }
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
    // The sub_workflow picker's options (issue #541). Same degrade-on-failure
    // shape as the roster: a host that can't list workflows leaves the field a
    // free-text id, never blocks authoring.
    (async () => {
      try {
        const list = await listWorkflows(client, company);
        if (live) setWorkflows(list);
      } catch {
        if (live) setWorkflows([]);
      }
    })();
    // Issue #813: the wired-channel picker's options. Same degrade-on-failure
    // shape — a host that can't list channels leaves the channel target a
    // free-text box rather than blocking authoring.
    (async () => {
      try {
        const channels = await listWiredChannels(client, company);
        if (live) setWiredChannels(channels);
      } catch {
        if (live) setWiredChannels([]);
      }
    })();
    return () => {
      live = false;
    };
  }, [open, client, company, workflow, prefilledDraft]);

  // Issue #753: check the company's cognition path when the copilot is on screen
  // (create mode). On the offline `echo` brain there is nothing to draft with, so
  // Draft disables; the check runs separately from the reset above so a slow
  // `/inference` read never delays clearing the form. Mirrors CopilotPanel:331.
  useEffect(() => {
    if (!open || editing) return;
    let live = true;
    setCognition(null);
    (async () => {
      try {
        const status = await getInferenceStatus(client, company);
        if (live) setCognition(status.cognition);
      } catch {
        // A host without the route tells us nothing either way — leave it enabled
        // rather than blocking a copilot that works. (`cognition` stays null,
        // which is not `echo`, so Draft is enabled.)
        if (live) setCognition(null);
      }
    })();
    return () => {
      live = false;
    };
  }, [open, editing, client, company]);

  /**
   * Retires the submit-time banner because the draft it described is gone
   * (issue #1005).
   *
   * `error` is the verdict on ONE graph — the client checks' first complaint or
   * the host's rejection — so it stops being true the moment the author changes
   * that graph. Left up it reads as the verdict on the graph now on screen, and
   * the next failed Create is indistinguishable from the last one: same text,
   * no new event.
   *
   * Every handler that mutates the draft calls this, structural ones included.
   * The banner routinely names something an add or a remove fixes ("Add at
   * least one node.", a duplicate node id, an edge pointing at nothing), so a
   * split where only the field edits clear it leaves the complaint up through
   * exactly the action that answers it. Field-level `fieldErrors` are NOT
   * touched here — those are scoped to their own control and each clears on its
   * own edit.
   */
  function clearSubmitError() {
    setError(null);
  }

  function addNode() {
    setNodes((rows) => [...rows, blankNode()]);
    clearSubmitError();
  }

  /** Edits the name and drops the stale submit banner with it — `validate()`
   * and the host both complain about the name, so an author fixing one is
   * looking straight at a complaint they have already addressed (#1005). */
  function changeName(value: string) {
    setName(value);
    clearSubmitError();
  }

  /** Same for the id, which is what `validate()` complains about FIRST (missing,
   * or not a safe id) and the most common 409 from the host — so it is the
   * banner an author is most often looking at while fixing its cause. */
  function changeId(value: string) {
    setId(value);
    clearSubmitError();
  }

  function changeDescription(value: string) {
    setDescription(value);
    clearSubmitError();
  }

  function updateNode(key: string, fields: Partial<DraftNode>) {
    setNodes((rows) => rows.map((r) => (r.key === key ? { ...r, ...fields } : r)));
    clearSubmitError();
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
      // A kind change resets the config draft (see `changeKind`), so this row's
      // config-field errors point at fields that are gone — drop them too.
      if ("kind" in fields) {
        const prefix = `${key}:config:`;
        for (const k of Object.keys(next)) {
          if (k.startsWith(prefix)) {
            delete next[k];
            changed = true;
          }
        }
      }
      return changed ? next : prev;
    });
  }

  /** Updates one config field's draft string and clears its own blur error —
   * the author is fixing it, same reasoning as {@link updateNode}. Kept
   * separate because config fields nest under `configDraft`, keyed by their
   * engine key rather than being top-level `DraftNode` fields (issue #541). */
  function updateConfigField(nodeKey: string, key: string, value: string) {
    setNodes((rows) =>
      rows.map((r) =>
        r.key === nodeKey ? { ...r, configDraft: { ...r.configDraft, [key]: value } } : r,
      ),
    );
    clearSubmitError();
    setFieldErrors((prev) => {
      const k = errorKey(nodeKey, `config:${key}`);
      if (!(k in prev)) return prev;
      const next = { ...prev };
      delete next[k];
      return next;
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
    let problem: string | null = null;
    if (field === "schedule") {
      problem = scheduleProblem(value);
    } else if (field === "destinationTarget") {
      problem = destinationTargetProblem(node.destinationKind, value, wiredChannels);
    } else if (field.startsWith("config:")) {
      const key = field.slice("config:".length);
      const spec = configFieldSpecs(node.kind).find((s) => s.key === key);
      if (spec) problem = configFieldProblem(spec, value);
    }
    if (problem) {
      setFieldErrors((prev) => ({ ...prev, [errorKey(nodeKey, field)]: problem }));
    }
  }

  function removeNode(key: string) {
    const removed = nodes.find((n) => n.key === key);
    setNodes((rows) => rows.filter((r) => r.key !== key));
    clearSubmitError();
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
    clearSubmitError();
  }

  function updateEdge(key: string, fields: Partial<DraftEdge>) {
    setEdges((rows) => rows.map((r) => (r.key === key ? { ...r, ...fields } : r)));
    clearSubmitError();
  }

  function removeEdge(key: string) {
    setEdges((rows) => rows.filter((r) => r.key !== key));
    clearSubmitError();
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
        wiredChannels,
      );
      if (destinationProblem) return `Node \`${nodeLabel(n)}\`: ${destinationProblem}`;
      // Kind-specific config (issue #541): required keys, malformed JSON, the
      // switch field-or-expression rule, a sub_workflow pointed at its own id.
      // Only ever checks a form kind, so it is a check on visible state.
      const configProblem = configDraftProblem(n.kind, n.id, n.configDraft);
      if (configProblem) return `Node \`${nodeLabel(n)}\`: ${configProblem}`;
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

  // Issue #274: fetch this workflow's edit history. Called on first expand and
  // again after a restore, so the list reflects the snapshot the restore just
  // captured. A host predating #274 has no such route; the panel degrades to
  // "no revisions" rather than throwing.
  async function loadRevisions() {
    if (!workflow) return;
    setRevisionsLoading(true);
    setRevisionsError(null);
    try {
      const rows = await listWorkflowRevisions(client, company, workflow.id);
      setRevisions(rows);
      setRevisionsLoaded(true);
    } catch (e) {
      setRevisionsError(
        e instanceof Error ? e.message : "could not load the edit history",
      );
    } finally {
      setRevisionsLoading(false);
    }
  }

  function toggleHistory() {
    const next = !historyOpen;
    setHistoryOpen(next);
    // Lazy: only fetch the first time it opens (or after a reset cleared it).
    if (next && !revisionsLoaded && !revisionsLoading) void loadRevisions();
  }

  // Issue #274: restore one snapshot. A confirm names the undoability, because a
  // restore overwrites the live graph — but the body it replaces is itself
  // snapshotted, so the operator can walk it back. On success the dialog
  // re-hydrates from the returned graph exactly as a save does (via `onSaved`),
  // so the canvas shows the restored body and the next edit carries its fresh
  // token; the history list is then refreshed to include the pre-restore body.
  async function restore(rev: WorkflowRevision) {
    if (!workflow || restoringId) return;
    const ok = window.confirm(
      `Restore "${rev.name}"? This replaces the current graph. The version you have now ` +
        `is saved to history first, so you can restore back to it.`,
    );
    if (!ok) return;
    setRestoringId(rev.id);
    setRevisionsError(null);
    try {
      const restored = await restoreWorkflowRevision(
        client,
        company,
        workflow.id,
        rev.id,
        // Condition on the graph the operator is looking at, so a concurrent
        // edit is a 409 rather than a silent clobber.
        workflow.version,
      );
      onSaved?.(restored);
      // The parent updates `workflow`, which re-hydrates this dialog and resets
      // the history state; re-fetch so the panel (if still open) reflects the
      // snapshot the restore just captured.
      await loadRevisions();
    } catch (e) {
      // 409 (moved under us) / 400 (invalid against the current graph) / 404 —
      // surface the host's prosumer-language message in the panel. A 409 also
      // rides out to the view's persistent reload banner, same as a save.
      setRevisionsError(
        e instanceof Error ? e.message : "could not restore this revision",
      );
      if (e instanceof ApiError && e.status === 409) onConflict?.(e.message);
    } finally {
      setRestoringId(null);
    }
  }

  // Issue #753: `echo` is the offline brain — there is no model to draft with, so
  // the copilot composer is disabled until the check settles onto a real path.
  const echoing = cognition === "echo";

  /** Whether the form holds anything a copilot draft would overwrite. The blank
   * starter — no id/name/description, no edges, one untouched `start` trigger —
   * is NOT dirty, so the first draft hydrates without a confirm; anything the
   * operator has already typed is, so it asks first. */
  function isDraftDirty(): boolean {
    if (id.trim() || name.trim() || description.trim()) return true;
    if (edges.length > 0) return true;
    if (nodes.length !== 1) return true;
    const only = nodes[0];
    return !(
      only.kind === "trigger" &&
      only.id === "start" &&
      only.name === "Start" &&
      !only.summary.trim() &&
      !only.agent.trim() &&
      !only.schedule.trim()
    );
  }

  /** Draft a graph from the description and hydrate the form with it (issue
   * #753). The hydrated, editable form IS the review surface — there is no
   * read-only diff — so on success the operator lands in the ordinary create
   * form with everything filled in, tweaks if needed, and presses Create. */
  async function runDraft() {
    const description = copilotPrompt.trim();
    if (!description || drafting || echoing) return;
    // Overwriting work the operator has already started is a confirm, not a
    // silent clobber — the same courtesy the History restore extends.
    if (
      isDraftDirty() &&
      !window.confirm(
        "Replace what you've started with the drafted workflow? You can still edit it before creating.",
      )
    ) {
      return;
    }
    setDrafting(true);
    setDraftError(null);
    setDraftSummary(null);
    setDraftReason(null);
    setDraftNotes([]);
    try {
      const drafted = await draftWorkflowFromDescription(client, company, description);
      const banners = draftBanners(drafted);
      if (drafted.automatable && drafted.workflow) {
        const graph = drafted.workflow;
        // Hydrate via the same helpers edit mode uses, so a drafted graph and a
        // saved one populate the form identically.
        setId(graph.id);
        setName(graph.name);
        setDescription(graph.description ?? "");
        setNodes(draftNodes(graph));
        setEdges(draftEdges(graph));
        // A fresh draft replaces the whole graph, so it clears both the submit
        // banner and the per-field blur errors — they belonged to whatever was
        // on screen before.
        clearSubmitError();
        setFieldErrors({});
        setDraftSummary(banners.summary);
        // Any host corrections (issue #813) — e.g. a role→id rewrite — so the
        // author sees WHY the drafted graph differs from a literal reading.
        setDraftNotes(banners.notes);
      } else {
        // Not automatable: the form is left untouched, with the model's reason.
        setDraftReason(banners.reason);
      }
    } catch (e) {
      // A capability gap (404/409) or a network failure — surface it inline; the
      // operator can still author by hand.
      setDraftError(e instanceof Error ? e.message : "could not draft a workflow");
    } finally {
      setDrafting(false);
    }
  }

  /**
   * Raise the submit-time banner and take the operator to it (issue #1005).
   *
   * Issue #813 gave the CLIENT-validation failure this treatment: the banner
   * sits inline in a scrollable dialog with the Create button below it, so on a
   * long graph the message lands off-screen and the button just looks dead.
   * A host rejection has exactly the same geometry and was getting none of it,
   * so both branches go through here rather than one of them setting `error`
   * on its own — the version that skips this is the one that reads as nothing
   * happening. The scroll/focus runs the frame AFTER the state change, because
   * `errorRef` points at a node that does not exist until the banner renders.
   */
  function showError(message: string) {
    setError(message);
    requestAnimationFrame(() => {
      errorRef.current?.scrollIntoView({ block: "center", behavior: "smooth" });
      errorRef.current?.focus();
    });
  }

  async function submit() {
    // Re-entrancy guard (issue #1005). The Create button disables while a write
    // is in flight, but `disabled` is a property of one DOM node: a second
    // activation landing in the same tick as the first, an Enter keypress, or
    // any future caller would otherwise post the graph twice — and for create
    // that means two workflows, or a 409 the operator did nothing to earn.
    //
    // It reads the REF, not `submitting`: the state value here is the one from
    // the render that built this closure, so two calls in the same tick would
    // both see `false` and the guard would pass twice. The ref is written below
    // before the first `await`, which makes it true for every later caller.
    if (submittingRef.current) return;
    const problem = validate();
    if (problem) {
      showError(problem);
      return;
    }
    // Set before the first `await` — after `validate()`, so a draft the client
    // rejects never latches the guard and the operator can fix it and retry.
    submittingRef.current = true;
    setSubmitting(true);
    setError(null);
    const graph: WorkflowGraph = {
      id: id.trim(),
      name: name.trim(),
      description: description.trim() || undefined,
      // Locally-built body; the conditional-write token is passed separately as
      // `expectedVersion`, so this carries none (issue #1013 makes it explicit).
      version: null,
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
          // Config: a form kind (#541) rebuilds it from its per-field draft
          // plus the preserved `extra` bag (so an edit keeps orchestrator keys
          // it has no control for); a form-less kind passes its raw overlay
          // straight back out — an edit must not delete what it cannot show.
          // `undefined` is omitted from the JSON body.
          config: hasConfigForm(n.kind)
            ? configFromDraft(n.kind, n.configDraft, n.configExtra)
            : n.config,
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
      showError(
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
      submittingRef.current = false;
      setSubmitting(false);
    }
  }

  return (
    <Dialog open={open} onOpenChange={(o) => !submitting && onOpenChange(o)}>
      {/* `aria-busy` while a save is in flight (issue #1005): the dialog stays
          on screen and mostly interactive during the round trip, so without it
          a screen reader has nothing to say the form is mid-write. */}
      <DialogContent
        className="max-h-[85vh] overflow-y-auto sm:max-w-2xl"
        aria-busy={submitting}
      >
        <DialogHeader>
          <DialogTitle>{editing ? "Edit workflow" : "New workflow"}</DialogTitle>
          <DialogDescription>
            {editing
              ? "Change the nodes, how they connect, or when it runs. Saving replaces the whole graph."
              : "Describe it and let the copilot draft it, or define the graph by hand — nodes, then how they connect."}
          </DialogDescription>
        </DialogHeader>

        {/* Issue #840 (PR-3): the fix-from-run banners. A copilot-corrected graph
            hydrated via `prefilledDraft` shows its summary, any host corrections,
            and the static readiness advisories — read-only, so the operator sees
            what changed and what still needs a look before Saving the new version.
            Rendered regardless of mode (a fix opens the dialog in edit mode), and
            only when a correction was handed in. */}
        {prefilledDraft && (
          <div className="space-y-2" data-testid="workflow-fix-banners">
            {draftSummary && (
              <Alert>
                <AlertDescription>{draftSummary}</AlertDescription>
              </Alert>
            )}
            {draftNotes.length > 0 && (
              <Alert data-testid="workflow-fix-notes">
                <AlertDescription>
                  <ul className="list-disc space-y-1 pl-4">
                    {draftNotes.map((note, i) => (
                      <li key={i}>{note}</li>
                    ))}
                  </ul>
                </AlertDescription>
              </Alert>
            )}
            {readiness && (
              <Alert
                variant={readiness.ok ? undefined : "destructive"}
                data-testid="workflow-fix-readiness"
              >
                <AlertDescription>
                  {readiness.ok ? (
                    "The corrected workflow passes the static authoring checks."
                  ) : (
                    <>
                      <p>
                        The copilot corrected the workflow, but a few authoring
                        checks still flag it — review before saving:
                      </p>
                      <ul className="mt-1 list-disc space-y-1 pl-4">
                        {(readiness.advisories ?? []).map((advisory, i) => (
                          <li key={i}>{advisory}</li>
                        ))}
                      </ul>
                    </>
                  )}
                </AlertDescription>
              </Alert>
            )}
          </div>
        )}

        {/* Issue #753: the create-time copilot. Create mode only — an edit
            already has a graph. It drafts a graph from a sentence and hydrates
            the form below with it; the operator reviews and edits in that form,
            then presses Create as usual. Nothing is saved by drafting. */}
        {!editing && (
          <div className="space-y-2 rounded-lg border bg-muted/30 p-3">
            <Label htmlFor={`${formId}-copilot`} className="flex items-center gap-2">
              <Sparkles className="size-4" />
              Describe the workflow
            </Label>
            <Textarea
              id={`${formId}-copilot`}
              rows={2}
              value={copilotPrompt}
              onChange={(e) => setCopilotPrompt(e.target.value)}
              placeholder="e.g. Every Monday morning, have the writer draft the weekly digest and email it to the team."
              // Also dead while a create is in flight (issue #1005): drafting
              // replaces the whole form, and the graph being posted is the one
              // on screen — so a draft landing mid-write would leave the
              // operator looking at a graph that is not the one they created.
              disabled={drafting || echoing || submitting}
            />
            <div className="flex items-center justify-between gap-2">
              <p className="text-2xs leading-snug text-muted-foreground">
                {echoing
                  ? "This company has no model configured, so the copilot can't draft yet — set one in Settings → Inference, or build the graph by hand below."
                  : "The copilot fills in the form below — review and edit it, then Create."}
              </p>
              <Button
                type="button"
                variant="outline"
                size="sm"
                onClick={() => void runDraft()}
                disabled={drafting || echoing || submitting || !copilotPrompt.trim()}
                data-testid="workflow-copilot-draft"
              >
                {drafting ? (
                  <Loader2 className="mr-1 size-3.5 animate-spin" />
                ) : (
                  <Sparkles className="mr-1 size-3.5" />
                )}
                {drafting ? "Drafting…" : "Draft it"}
              </Button>
            </div>
            {draftSummary && (
              <Alert>
                <AlertDescription>{draftSummary}</AlertDescription>
              </Alert>
            )}
            {draftNotes.length > 0 && (
              <Alert data-testid="workflow-copilot-notes">
                <AlertDescription>
                  <ul className="list-disc space-y-1 pl-4">
                    {draftNotes.map((note, i) => (
                      <li key={i}>{note}</li>
                    ))}
                  </ul>
                </AlertDescription>
              </Alert>
            )}
            {draftReason && (
              <Alert>
                <AlertDescription>{draftReason}</AlertDescription>
              </Alert>
            )}
            {draftError && (
              <Alert variant="destructive">
                <AlertDescription>{draftError}</AlertDescription>
              </Alert>
            )}
          </div>
        )}

        {/*
          Every control that can change the draft goes dead while a save is in
          flight (issue #1005). `submit()` snapshots the graph before it awaits,
          so an edit landing during the round trip is in neither the request nor
          the result — and on success the dialog closes, taking that edit with
          it. The operator would have watched themselves type it.

          A `fieldset` rather than a `disabled` prop threaded through `NodeRow`,
          `EdgeRow`, `ScheduleField` and `NodeConfigFields`: `disabled`
          propagates natively to every form control underneath, so a control
          added later is covered by construction rather than by remembering.
          `display: contents` keeps it out of the layout — the grids below still
          see their own children.
        */}
        <fieldset disabled={submitting} className="contents">
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
                onChange={(e) => changeId(e.target.value)}
                readOnly={editing}
                aria-readonly={editing || undefined}
                className={editing ? "text-muted-foreground" : undefined}
                placeholder="e.g. campaign_pipeline"
              />
              {editing && (
                <p className="text-2xs leading-snug text-muted-foreground">
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
                onChange={(e) => changeName(e.target.value)}
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
              onChange={(e) => changeDescription(e.target.value)}
              placeholder="What does this workflow do?"
            />
          </div>

          <div className="space-y-2">
            <div className="flex items-center justify-between">
              <Label>Nodes</Label>
              {/* Off while a save is in flight (issue #1005): the graph being
                  posted is the one on screen, and a row added mid-write is in
                  neither the request nor the result. */}
              <Button
                type="button"
                variant="outline"
                size="sm"
                onClick={addNode}
                disabled={submitting}
              >
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
                  wiredChannels={wiredChannels}
                  workflows={workflows}
                  createMode={!editing}
                  errors={{
                    schedule: fieldErrors[errorKey(n.key, "schedule")],
                    destinationTarget: fieldErrors[errorKey(n.key, "destinationTarget")],
                  }}
                  configErrors={Object.fromEntries(
                    configFieldSpecs(n.kind).map((s) => [
                      s.key,
                      fieldErrors[errorKey(n.key, `config:${s.key}`)],
                    ]),
                  )}
                  onValidateField={(field, value) => validateField(n.key, field, value)}
                  onConfigChange={(key, value) => updateConfigField(n.key, key, value)}
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
                disabled={nodes.length < 2 || submitting}
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
        </fieldset>

        {error && (
          // Wrapper carries the ref/focus target so it works regardless of
          // whether `Alert` forwards a ref (#813 defect 6).
          <div
            ref={errorRef}
            tabIndex={-1}
            // Announced, not merely rendered (issue #1005): `assertive`
            // because the write the operator just asked for did not happen,
            // and `showError`'s focus move is the only other thing that
            // reports it. `role="alert"` already implies the live region;
            // both are stated so a later refactor of one does not silently
            // take the other with it.
            role="alert"
            aria-live="assertive"
            data-testid="create-error"
            className="outline-none"
          >
            <Alert variant="destructive">
              <AlertDescription>{error}</AlertDescription>
            </Alert>
          </div>
        )}

        {/* Issue #274: the edit-history panel. Edit mode only — a workflow being
            created has nothing to look back on. */}
        {editing && (
          <div className="rounded-lg border">
            <button
              type="button"
              onClick={toggleHistory}
              className="flex w-full items-center justify-between gap-2 px-3 py-2 text-sm font-medium"
              aria-expanded={historyOpen}
              data-testid="workflow-history-toggle"
            >
              <span className="flex items-center gap-2">
                <History className="size-4" />
                History
              </span>
              <span className="text-xs text-muted-foreground">
                {historyOpen ? "Hide" : "Show"}
              </span>
            </button>
            {historyOpen && (
              <div className="border-t px-3 py-2">
                {revisionsLoading && (
                  <p className="py-2 text-center text-xs text-muted-foreground">
                    Loading history…
                  </p>
                )}
                {revisionsError && (
                  <Alert variant="destructive" className="my-2">
                    <AlertDescription>{revisionsError}</AlertDescription>
                  </Alert>
                )}
                {!revisionsLoading && !revisionsError && revisions.length === 0 && (
                  <p className="py-2 text-center text-xs text-muted-foreground">
                    No earlier versions yet — every edit you save will show up here.
                  </p>
                )}
                <ul className="divide-y">
                  {revisions.map((rev) => (
                    <li
                      key={rev.id}
                      className="flex items-center justify-between gap-3 py-2"
                    >
                      <div className="min-w-0">
                        <p className="truncate text-sm">{rev.name}</p>
                        <p className="text-2xs text-muted-foreground">
                          {relativeTime(rev.createdAtMillis)}
                        </p>
                      </div>
                      <Button
                        type="button"
                        variant="outline"
                        size="sm"
                        onClick={() => void restore(rev)}
                        disabled={restoringId !== null || submitting}
                        aria-label={`Restore ${rev.name}`}
                      >
                        {restoringId === rev.id ? (
                          <Loader2 className="mr-1 size-3.5 animate-spin" />
                        ) : (
                          <RotateCcw className="mr-1 size-3.5" />
                        )}
                        {restoringId === rev.id ? "Restoring…" : "Restore"}
                      </Button>
                    </li>
                  ))}
                </ul>
              </div>
            )}
          </div>
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
            {submitting && <Loader2 className="mr-1.5 size-4 animate-spin" />}
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
    <p id={id} className="text-2xs leading-snug text-destructive">
      {message}
    </p>
  );
}

function NodeRow({
  node,
  client,
  company,
  roster,
  wiredChannels,
  workflows,
  createMode,
  errors,
  configErrors,
  onValidateField,
  onConfigChange,
  onChange,
  onRemove,
}: {
  node: DraftNode;
  /** Threaded through solely so the trigger row's schedule field can ask the
   * host what its cron means (issue #262). */
  client: OpenCompanyClient;
  company: string | null;
  roster: TeamMemberDto[];
  /** The company's wired chat channels (#813): the output-node channel-destination
   * picker's options. Empty → the channel target degrades to a free-text box. */
  wiredChannels: string[];
  /** The company's workflows, for a `sub_workflow` node's picker (issue #541). */
  workflows: WorkflowSummary[];
  /** True while creating a new workflow (not editing an existing one), so the
   * trigger row can disclose that a scheduled workflow is created paused (#813). */
  createMode: boolean;
  /** Blur-time problems for this row's validated fields, if any. */
  errors: Partial<Record<ValidatedField, string>>;
  /** Blur-time problems for this row's config fields, keyed by engine key. */
  configErrors: Record<string, string | undefined>;
  onValidateField: (field: ValidatedField, value: string) => void;
  onConfigChange: (key: string, value: string) => void;
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
            createMode={createMode}
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
                {/* base-ui renders the raw stored value in the collapsed
                    control unless given text, which surfaced the bare
                    `__none__` sentinel; map every value to its label. #813 */}
                <SelectValue>
                  {destinationLabel(node.destinationKind || NO_DESTINATION)}
                </SelectValue>
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
            {node.destinationKind === "channel" && wiredChannels.length > 0 && (
              <>
                {/* #813: pick from the channels this company can actually
                    deliver to, instead of a free-text box that only fails at
                    delivery time. #981: the host no longer includes `operator`
                    in that list — it is an in-memory response surface with no
                    durable reader, and delivery refuses it by name — so the
                    picker can no longer offer it. */}
                <Select
                  value={node.destinationTarget || ""}
                  onValueChange={(v) => {
                    onChange({ destinationTarget: v ?? "" });
                    onValidateField("destinationTarget", v ?? "");
                  }}
                >
                  <SelectTrigger className="h-8" aria-label="Channel id">
                    <SelectValue placeholder="pick a wired channel" />
                  </SelectTrigger>
                  <SelectContent>
                    {wiredChannels.map((channelId) => (
                      <SelectItem key={channelId} value={channelId}>
                        {channelId}
                      </SelectItem>
                    ))}
                  </SelectContent>
                </Select>
                <FieldError id={targetErrorId} message={errors.destinationTarget} />
              </>
            )}
            {(node.destinationKind === "email" ||
              (node.destinationKind === "channel" && wiredChannels.length === 0)) && (
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
              <p className="text-2xs leading-snug text-muted-foreground">
                Only sends if this company grants email and the recipient has
                already written in.
              </p>
            )}
            {/* #981: an empty list is now a legitimate answer — a company with
                no desks and no connected channels has nowhere to post a report,
                so the free-text box above is the honest fallback rather than a
                picker of one bad option. Say why it is empty; without this the
                author sees a blank box and no reason. */}
            {node.destinationKind === "channel" && wiredChannels.length === 0 && (
              <p className="text-2xs leading-snug text-muted-foreground">
                No delivery channels are wired for this company — add a desk, or
                connect a channel, before a report can be posted to one.
              </p>
            )}
          </>
        )}
        {/* The five kinds that need config to run (issue #541). Rendered here,
            alongside the trigger's schedule and the output's destination, so a
            node's kind-specific controls all live in the same column. */}
        {hasConfigForm(node.kind) && (
          <NodeConfigFields
            idPrefix={rowId}
            kind={node.kind}
            draft={node.configDraft}
            errors={configErrors}
            workflows={workflows}
            selfId={node.id}
            onChange={onConfigChange}
            onValidate={(key, value) => onValidateField(`config:${key}`, value)}
          />
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
  createMode,
  schedule,
  error,
  onChange,
  onBlurValidate,
}: {
  client: OpenCompanyClient;
  company: string | null;
  /** True on a new workflow, to disclose the created-paused default (#813). */
  createMode: boolean;
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
        <p className="text-3xs text-muted-foreground">
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
      {/* A scheduled workflow is disarmed on create (#276 disarm rule,
          src/company/workflow_create.rs); nothing else in the dialog says so,
          so an author who sets a cron here would think it is armed. Disclose it
          at author time — create mode only, and only once a real schedule is
          set. #813 */}
      {createMode && looksLikeCron(schedule) && (
        <p className="text-3xs text-muted-foreground">
          Heads up: a scheduled workflow is created paused. Resume it from the
          list to arm the schedule.
        </p>
      )}
    </div>
  );
}

/**
 * A compact "how long ago" label for a revision row (issue #274). Coarse on
 * purpose — the history list wants "2h ago", not a timestamp — and it falls back
 * to a locale date past a week so an old snapshot reads as a real date.
 */
function relativeTime(millis: number): string {
  const secs = Math.max(0, Math.round((Date.now() - millis) / 1000));
  if (secs < 60) return "just now";
  const mins = Math.round(secs / 60);
  if (mins < 60) return `${mins}m ago`;
  const hours = Math.round(mins / 60);
  if (hours < 24) return `${hours}h ago`;
  const days = Math.round(hours / 24);
  if (days < 7) return `${days}d ago`;
  return new Date(millis).toLocaleDateString();
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
