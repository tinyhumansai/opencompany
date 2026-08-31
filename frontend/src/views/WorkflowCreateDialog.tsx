// The workflow creator (issue #69): a plain form editor — not a drag canvas —
// that builds a `WorkflowGraph` and posts it via `createWorkflow`. Node kinds
// are the ones the engine executes and the console can author from a form
// (`NODE_KINDS`). The five that need kind-specific config —
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

import { useCallback, useEffect, useId, useRef, useState } from "react";
import { History, Loader2, Plus, RotateCcw, Sparkles, Trash2 } from "lucide-react";

import {
  NODE_KINDS,
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
  validateWorkflow,
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
import { draftBanners, draftLanding } from "@/lib/workflow-draft";
import { isSafeId, slugifyWorkflowId } from "@/lib/workflow-id";
import type { OpenCompanyClient } from "@/api/client";
import { ApiError } from "@/api/types";
import { CronPreviewLine } from "@/views/CronPreviewLine";
import { NodeConfigFields } from "@/views/workflows/NodeConfigFields";
import type { TeamMemberDto } from "@/api/types";
import { Alert, AlertDescription } from "@/components/ui/alert";
import {
  AlertDialog,
  AlertDialogAction,
  AlertDialogCancel,
  AlertDialogContent,
  AlertDialogDescription,
  AlertDialogFooter,
  AlertDialogHeader,
  AlertDialogTitle,
} from "@/components/ui/alert-dialog";
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
 * user-editable `id` field (which can be blank or duplicated mid-edit).
 * Exported for direct unit testing, same as {@link WiredChannels}. */
export interface DraftNode {
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
  /**
   * Issue #850. Carried but not authored here: an operator sets it through the
   * write route, and this dialog must round-trip it rather than drop it — a
   * lost `repeatable: false` is a repeat guard removed by an unrelated edit,
   * the same hazard as a dropped `requiresApproval`.
   */
  repeatable?: boolean;
  /**
   * Issue #1866. Same "carried but not authored" contract as `repeatable`
   * above: an operator sets a postcondition through the write route (agent
   * nodes only today), this dialog has no control for it, and dropping it on
   * an unrelated edit silently removes a run-safety gate rather than merely
   * an operational-tuning field (issue #1937 review).
   */
  postcondition?: WorkflowNode["postcondition"];
}

/** How long the graph must sit still before the host is asked about it (issue
 * #1074). Long enough that dragging an edge or renaming a node does not spend a
 * round trip per keystroke; short enough that the verdict is there before an
 * author who finished editing reaches for Create. */
const PREFLIGHT_DEBOUNCE_MS = 700;

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

/** What the console knows about the channels this company can deliver to
 * (issue #981).
 *
 * Three states, deliberately not collapsed into `string[]`. They used to be:
 * `[]` meant "loading", "the host has no such route", and "the host answered,
 * this company has nowhere to deliver" all at once, and the pre-flight treated
 * all three as "don't check". Only the last of those is knowledge, and it is
 * the one the host refuses a channel target against — so the console skipped
 * exactly the check the host applies, and skipped it on a timer.
 *
 * - `loading` — the request is in flight. We know nothing YET, and the answer
 *   is coming: {@link destinationCheckDeferred} holds Save rather than passing.
 * - `unavailable` — the request failed, or the host predates the route. We know
 *   nothing and never will, so the channel target degrades to free text and the
 *   host's save-time 400 is the only gate. Never blocks Save.
 * - `ready` — the host answered. `ids` is the answer, **including `[]`**, which
 *   means this company genuinely has nowhere to post a report.
 */
export type WiredChannels =
  | { status: "loading" }
  | { status: "unavailable" }
  | { status: "ready"; ids: string[] };

/** The `ids` a picker may offer: only a settled, non-empty answer has any. */
function channelOptions(channels: WiredChannels): string[] {
  return channels.status === "ready" ? channels.ids : [];
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
  channels: WiredChannels,
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
  // catch it at author time instead.
  //
  // #981: the same sentence the host's save-time rejection carries, so an author
  // who trips the pre-flight and an author who trips the 400 are told the same
  // thing. `operator` used to be excluded from the list the host serves, since
  // it was an in-memory response surface delivery refused by name — but since
  // #1757 it is a durable, journal-backed channel every company wires, so it is
  // a real target like any other and this check accepts it the same way.
  //
  // Gated on `status === "ready"`, NOT on the list being non-empty. Those were
  // the same test until the host started answering `[]` for a company with no
  // desks and no connected channels (#981): that list is knowledge, and the
  // host refuses every channel target against it, so the pre-flight must too.
  // While the answer is still in flight, or when the request failed, we know
  // nothing — {@link destinationCheckDeferred} is what keeps that from reading
  // as a pass.
  if (
    kind === "channel" &&
    value &&
    channels.status === "ready" &&
    !channels.ids.includes(value)
  ) {
    return `\`${value}\` is not a workflow delivery channel — this runtime has: ${
      channels.ids.length > 0 ? channels.ids.join(", ") : "no durable channels"
    }.`;
  }
  return null;
}

/** Why the channel pre-flight cannot answer yet, or `null` when it can (or when
 * there is nothing for it to check).
 *
 * Issue #981: {@link destinationTargetProblem} has to return `null` while the
 * wired-channel list is loading — it genuinely does not know — and `null` is
 * indistinguishable from "checked and fine". That made the check a race: an
 * author who hit Save before the fetch settled got no pre-flight at all, while a
 * slower one got the full one. Same draft, different validation, decided by
 * network timing.
 *
 * So `validate()` asks this FIRST and defers instead: Save is held for as long
 * as the answer is genuinely in flight, and released the moment it lands. A
 * failed or absent request settles as `unavailable`, never `loading`, so a host
 * that cannot answer degrades to free text and the host's own 400 rather than
 * blocking the save forever.
 *
 * Not wired into the blur handler: a field is blurred constantly, and "still
 * checking" is not a mistake the author made.
 */
export function destinationCheckDeferred(
  kind: DraftNode["destinationKind"],
  target: string,
  channels: WiredChannels,
): string | null {
  if (kind !== "channel" || !target.trim() || channels.status !== "loading") {
    return null;
  }
  return "still checking which channels this company can deliver to — try Save again in a moment.";
}

/** The host's standing answer about the graph on screen (issue #1074). */
export type Preflight =
  | { status: "idle" }
  /** A request is in flight for `key`. */
  | { status: "asking"; key: string }
  /** The host would accept the graph `key` describes. */
  | { status: "ok"; key: string }
  /** The host would refuse it, in its own words. */
  | { status: "refused"; key: string; message: string }
  /** The ask itself failed (offline, host too old to know the route). Not a
   * verdict on the graph, and never shown as one. */
  | { status: "unavailable"; key: string };

/**
 * Whether a {@link Preflight} still describes the graph now on screen.
 *
 * The verdict is the host's answer about ONE body. The author keeps typing, so
 * by the time it lands the question may have changed — and a stale "looks good"
 * is exactly the false green #1048 was about, one layer up. Rendering is gated
 * on this rather than on the request having finished.
 */
export function preflightIsCurrent(preflight: Preflight, key: string): boolean {
  return preflight.status !== "idle" && preflight.key === key;
}

/** The draft the dialog holds, as {@link assembleGraph} reads it. */
export interface GraphDraft {
  id: string;
  name: string;
  description: string;
  /**
   * The owning desk (issue #1862 prerequisite), carried through unedited —
   * see {@link WorkflowGraph.ownerDesk}. This dialog has no control for it;
   * it exists on the draft purely so a Save round-trips whatever the loaded
   * graph had instead of dropping it (issue #1882 review).
   */
  ownerDesk?: string;
  nodes: DraftNode[];
  edges: DraftEdge[];
}

/** {@link assembleGraph}'s answer: the body to send, or the node whose config
 * could not be serialized and why. */
export type AssembledGraph =
  | { ok: true; graph: WorkflowGraph }
  | { ok: false; node: DraftNode; error: string };

/**
 * Builds the `WorkflowGraph` body from the draft rows — the exact body Create
 * and Save post.
 *
 * Extracted from `submit()` so the host pre-flight (`POST …/workflows/validate`)
 * can ask about **the same bytes** the submit will send. A second assembly
 * written for the pre-flight would be a mirror, and a pre-flight that validates
 * something other than what is submitted is worse than none — the whole subject
 * of #1074.
 *
 * Pure: it reads the draft and returns a value. The caller decides what a
 * serialization failure means (`submit()` shows it and stops; the pre-flight
 * stays quiet, because the client checks have not run yet on a half-typed row).
 */
export function assembleGraph(draft: GraphDraft): AssembledGraph {
  const outNodes: WorkflowNode[] = [];
  for (const n of draft.nodes) {
    // Config: a form kind (#541) rebuilds it from its per-field draft plus the
    // preserved `extra` bag (so an edit keeps orchestrator keys it has no
    // control for); a form-less kind passes its raw overlay straight back out —
    // an edit must not delete what it cannot show. `undefined` is omitted from
    // the JSON body.
    let config: unknown = n.config;
    if (hasConfigForm(n.kind)) {
      const serialized = configFromDraft(n.kind, n.configDraft, n.configExtra);
      if (!serialized.ok) return { ok: false, node: n, error: serialized.error };
      config = serialized.config;
    }
    outNodes.push({
      id: n.id.trim(),
      kind: n.kind,
      name: n.name.trim(),
      summary: n.summary.trim() || undefined,
      agent: n.kind === "agent" ? n.agent.trim() : undefined,
      // The host rejects a schedule on any non-trigger node, so only the
      // trigger's value is ever sent.
      schedule: n.kind === "trigger" && n.schedule.trim() ? n.schedule.trim() : undefined,
      // Only output nodes route a report, and `owner` resolves server-side so it
      // must carry no target — the host rejects one.
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
      config,
      onError: n.onError,
      retry: n.retry,
      requiresApproval: n.requiresApproval,
      // Round-trips a node the operator marked as never-repeatable (issue #850):
      // a dropped `repeatable: false` is a repeat guard removed by an unrelated
      // edit, the same hazard as a dropped `requiresApproval`.
      repeatable: n.repeatable,
      // Round-trips a declared postcondition (issue #1866) the same way —
      // this dialog has no control for it, so a Save must not silently clear
      // a run-safety gate an operator set through the write route.
      postcondition: n.postcondition,
    });
  }
  return {
    ok: true,
    graph: {
      id: draft.id.trim(),
      name: draft.name.trim(),
      description: draft.description.trim() || undefined,
      // No control edits this — carried through exactly as loaded, so Save
      // never clears an owner this dialog can't show (issue #1882 review).
      ownerDesk: draft.ownerDesk,
      // Locally-built body; the conditional-write token is passed separately as
      // `expectedVersion`, so this carries none (issue #1013 makes it explicit).
      version: null,
      nodes: outNodes,
      edges: draft.edges.map(
        (e): WorkflowEdge => ({
          from: e.from.trim(),
          to: e.to.trim(),
          label: e.label.trim() || undefined,
        }),
      ),
    },
  };
}

/** An edge endpoint as {@link EdgeRow} needs it: the id it offers in the two
 * pickers, plus the two fields the host's branch-label rule keys off. */
export interface EdgeEndpoint {
  id: string;
  kind: string;
  /** Carried verbatim off the node row (see {@link DraftNode.config}); the
   * dialog has no control for it, so it can only be read, never assumed. */
  onError?: string;
}

/** The branch labels the host accepts on an edge leaving a `condition` node.
 * Protocol strings, not display text: the host matches on these values, so they
 * are never translated. */
const CONDITION_BRANCHES = ["yes", "no"] as const;

/** What {@link EdgeRow} should render for one edge's label field. */
export interface BranchChoice {
  /** The options to offer, in order. */
  options: string[];
  /** The option to show as chosen; `""` when the row has no label yet. */
  value: string;
  /** Set when the host would refuse this row as it stands. The value is left
   * alone — it is shown, not corrected. */
  problem: string | null;
}

/**
 * The label control for the edge leaving `source` — a fixed set of branches when
 * that node is a `condition`, or `null` meaning "any label is legal here, keep
 * the free-text input".
 *
 * # Why this is an affordance and not a validator
 *
 * The host refuses an edge out of a `condition` unless its label reads `yes` or
 * `no` — or exactly `error`, and only when that node is also `on_error =
 * "route"` (`src/company/workflow_create.rs`). Issue #1074 is about a dialog that
 * could not pre-empt that rule without *mirroring* it, and a mirrored rule
 * drifts. This does not mirror it: it offers only values the host accepts, so it
 * can never invent a refusal the host would not make. It can only fail to offer
 * something, and the host then says so in its own words. The host stays the
 * authority — see `POST …/workflows/validate`, the other half of #1074.
 *
 * # Matching, exactly as the host matches
 *
 * `yes`/`no` are compared trimmed and lowercased, so a graph that stored `Yes`
 * is represented by the `yes` option rather than reported as unrepresentable.
 * `error` is compared **verbatim**, because the host compares it verbatim. A
 * label neither rule accepts is returned as-is with a `problem`: an existing
 * graph can legally carry one (`parse_workflow` is lenient on this rule since
 * issue #682, so a pre-#661 graph still loads), and quietly rewriting an
 * author's label to a legal one is a worse answer than showing it to them.
 */
export function conditionBranchChoice(
  source: EdgeEndpoint | undefined,
  label: string,
): BranchChoice | null {
  if (!source || source.kind !== "condition") return null;
  const options: string[] =
    source.onError === "route" ? [...CONDITION_BRANCHES, "error"] : [...CONDITION_BRANCHES];
  // Verbatim, like the host: `Error` is not the recovery branch.
  if (label === "error" && options.includes("error")) {
    return { options, value: "error", problem: null };
  }
  const folded = label.trim().toLowerCase();
  if (folded === "yes" || folded === "no") {
    return { options, value: folded, problem: null };
  }
  if (!label.trim()) {
    return {
      options,
      value: "",
      problem: `pick a branch — an edge out of \`${source.id}\` must be labeled ${options
        .map((o) => `\`${o}\``)
        .join(" or ")}.`,
    };
  }
  return {
    options,
    value: label,
    problem: `\`${label}\` is not a branch of \`${source.id}\` — it must be labeled ${options
      .map((o) => `\`${o}\``)
      .join(" or ")}.`,
  };
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

export interface DraftEdge {
  key: string;
  from: string;
  to: string;
  label: string;
}

/**
 * Add only the missing adjacent edges for the node order shown in the form.
 * Invalid/duplicate ids leave the explicit graph untouched so this convenience
 * never manufactures a self-edge or guesses which duplicate row was intended.
 */
export function edgesConnectingNodesInOrder(
  nodes: readonly Pick<DraftNode, "id">[],
  edges: readonly DraftEdge[],
): DraftEdge[] {
  const ids = nodes.map((node) => node.id.trim());
  if (ids.length < 2 || ids.some((nodeId) => !nodeId) || new Set(ids).size !== ids.length) {
    return [...edges];
  }
  const next = [...edges];
  for (let index = 0; index < ids.length - 1; index += 1) {
    const from = ids[index];
    const to = ids[index + 1];
    if (!next.some((edge) => edge.from === from && edge.to === to)) {
      next.push({ key: nextKey(), from, to, label: "" });
    }
  }
  return next;
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

/** What the operator is asked before unsaved graph edits are thrown away
 * (issue #1006). One string, because every path out of the dialog — Esc, a
 * click outside, Cancel, a hash navigation — has to ask the same question. */
const DISCARD_PROMPT =
  "You have unsaved changes to this workflow. Leave without saving them?";

/**
 * A stable string covering everything the form can change (issue #1006).
 *
 * "Has the operator edited anything?" is then ONE comparison against the value
 * this open hydrated from, rather than a field-by-field diff that goes quietly
 * wrong the moment `DraftNode` grows a member — which is how half an hour of
 * graph edits got discarded without a prompt in the first place.
 *
 * `key` is excluded on purpose: it is a React row identity handed out by
 * `nextKey()`, so it differs between two hydrations of the same graph and would
 * report every freshly-opened dialog as dirty.
 */
function draftFingerprint(
  id: string,
  name: string,
  description: string,
  nodes: DraftNode[],
  edges: DraftEdge[],
): string {
  return JSON.stringify({
    id,
    name,
    description,
    nodes: nodes.map(({ key: _key, ...rest }) => rest),
    edges: edges.map(({ key: _key, ...rest }) => rest),
  });
}

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
export function changeKind(kind: string): Partial<DraftNode> {
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
    // `repeatable` is valid on `tool_call`/`http_request` only (issue #850);
    // the host rejects it on every other kind. This dialog has no control to
    // author or clear it — it only round-trips a value set through the write
    // route (see `DraftNode.repeatable`) — so a kind change is the one place
    // left to reset it, same as `config` above: unconditionally, on ANY kind
    // change including between the two kinds that could both hold it, so
    // there is one rule rather than a kind-pair special case. Otherwise
    // switching away from a call node leaves a value `submit()` still sends,
    // and the save fails on a field the author can no longer see.
    repeatable: undefined,
    // Same reasoning as `repeatable` immediately above, for the same reason:
    // a postcondition (issue #1866) is valid on `agent` nodes only, this
    // dialog has no control to author or clear it, and a kind change is the
    // one place left to reset a value that would otherwise survive onto a
    // kind the host rejects it on.
    postcondition: undefined,
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
 *
 * Exported for the tests, the same reason {@link assembleGraph} is: proving a
 * carried-but-not-authored field (issue #1866 review, #1937) survives a
 * round trip needs THIS function, not a hand-built `GraphDraft` that could
 * never reproduce the read-side half of the bug.
 */
export function draftNodes(graph: WorkflowGraph): DraftNode[] {
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
      repeatable: n.repeatable,
      postcondition: n.postcondition,
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
  // No control in this dialog sets this — see {@link GraphDraft.ownerDesk}.
  // Held purely so a Save carries forward whatever the loaded graph had.
  const [ownerDesk, setOwnerDesk] = useState<string | undefined>(undefined);
  const [nodes, setNodes] = useState<DraftNode[]>(starterNodes());
  const [edges, setEdges] = useState<DraftEdge[]>([]);
  const [roster, setRoster] = useState<TeamMemberDto[]>([]);
  /** The chat channels this company can actually deliver to (#813): the picker
   * options for an output node's `channel` destination. Degrades to a free-text
   * box when the host offers no list, so authoring is never blocked. Carries
   * its own load status (#981) — see {@link WiredChannels} for why an empty
   * list and an unanswered request must not be the same value. */
  const [wiredChannels, setWiredChannels] = useState<WiredChannels>({
    status: "loading",
  });
  /** The company's workflows, for the `sub_workflow` config picker (#541). The
   * graph's own id is dropped at render time — a sub-workflow can't call
   * itself. Degrades to a free-text id field when the host offers no list. */
  const [workflows, setWorkflows] = useState<WorkflowSummary[]>([]);
  /** The host's standing verdict on the graph as it is now (issue #1074). See
   * {@link Preflight} and the effect below for when it is asked for. */
  const [preflight, setPreflight] = useState<Preflight>({ status: "idle" });
  const [submitting, setSubmitting] = useState(false);
  /** The same "a write is in flight" fact as `submitting`, held where `submit()`
   * can read it SYNCHRONOUSLY (issue #1005). `submitting` is captured from the
   * render that produced the handler, so two calls landing before React commits
   * `setSubmitting(true)` — a double activation, an Enter keypress arriving with
   * a click, any caller added later — would both read `false` and both post. The
   * state stays because the render needs it; the ref is what actually guards. */
  const submittingRef = useRef(false);
  /**
   * Whether the create-time id confirm is on screen (issue #1808).
   *
   * The id is a permanent backend join key — it keys the overlay body, the
   * revision store, the scheduler's armed state, run history, and cross-graph
   * `sub_workflow` references — so the host answers 400 to a rename. Creation is
   * the only moment it can be set, and in create mode it is silently derived
   * from the name, so a name typo becomes a permanent id with no acknowledgement.
   * `submit()` gates on this in create mode: the first Create shows the confirm,
   * the confirm's own action runs the write. Never true in edit mode — the id is
   * fixed there and the form field is read-only.
   */
  const [confirmingId, setConfirmingId] = useState(false);
  const [error, setError] = useState<string | null>(null);
  /** The submit-time error banner, so a failed submit can scroll it into view
   * and focus it rather than leave the message off-screen (#813 defect 6). */
  const errorRef = useRef<HTMLDivElement>(null);
  /**
   * Whether the id is the operator's to own (issue #1053).
   *
   * `false` means the field is still derivable and the name may keep writing it;
   * `true` means somebody decided it — the operator typed one, or a copilot
   * draft supplied one — and the name must stop touching it. **Clobbering a
   * deliberate id is a worse bug than the one being fixed**, so this latches on
   * and only resets when the dialog reopens.
   */
  const [idTouched, setIdTouched] = useState(false);
  /**
   * Write an id somebody **chose** — the operator, a copilot draft, a prefilled
   * correction — and latch it against derivation in the same step.
   *
   * One doorway on purpose (issue #1053 review). The latch was originally
   * applied at each site that set a chosen id, and `runDraft`'s hydrate was
   * missed: a create-mode draft landed its id and the next keystroke in Name
   * slugged over it. Pairing the two writes here means a future path cannot set
   * a chosen id *without* claiming it — the bug is removed as a class, not as an
   * instance. Outside the reset effect, bare `setId` now means exactly one
   * thing: a derived id. The reset effect is the one exception and has to be —
   * it hydrates every field through `next*` locals so the pristine fingerprint
   * is taken from the values it applies (issue #1006), so its id write cannot
   * go through here. It sets `idTouched` itself, in the same pass.
   */
  function setAuthoredId(next: string) {
    setIdTouched(true);
    setId(next);
  }
  /**
   * Identity of the dialog's current contents (issue #1052).
   *
   * Bumped by the reset effect below, i.e. on every open and every re-hydrate.
   * `runDraft` captures it before its request and compares after, so a draft the
   * operator walked away from cannot hydrate whatever is on screen later. The
   * file's other async paths use an effect-scoped `live` flag; an event handler
   * has no cleanup to flip, so the same idea is carried on a ref.
   */
  const draftEpochRef = useRef(0);
  /**
   * Whether the form holds operator work, readable **after** an await.
   *
   * `isDraftDirty()` closes over its render's state, so calling it again when a
   * response lands re-reads the values captured when the request was issued —
   * it would look like a re-check and answer the old question. This ref is
   * refreshed on every render, so the post-await read sees what is on screen now.
   */
  const draftDirtyRef = useRef(false);
  /**
   * The current node rows, readable **after** an await (issue #1016).
   *
   * `submit()` captures `nodes` from the render that built its closure, but the
   * operator is invited to keep editing through the in-flight write. When the
   * host answers with node-scoped `problems`, each one has to be matched against
   * the rows on screen NOW — a `node_id` the operator renamed since clicking
   * Save must fall through to the flat banner, not silently miss. Refreshed on
   * every render below, mirroring `draftDirtyRef`.
   */
  const nodesRef = useRef<DraftNode[]>([]);
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
  /** The fingerprint of the draft as this open hydrated it (issue #1006).
   * Rewritten by the hydration effect below — which is the only place the form
   * is populated from something other than the operator — so a re-hydrate (the
   * conflict banner's Reload, a History restore) resets the baseline instead of
   * leaving the dialog permanently "dirty". */
  const pristineRef = useRef("");

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
    // Issue #1053: a fresh open starts derivable again. Edit mode and the
    // prefilled-draft branch below both re-latch it, because both arrive with an
    // id somebody already chose.
    setIdTouched(Boolean(workflow));
    // Issue #1052: a draft still in flight belongs to the contents being
    // replaced right now, not to these. Bumping first is what makes its
    // response land as `drop` instead of overwriting a freshly-reset form.
    draftEpochRef.current += 1;
    // …and the button it disabled belongs to that abandoned request too, or a
    // reopened dialog starts with Draft it inert until a request nobody is
    // waiting for settles.
    setDrafting(false);
    // Held as locals rather than read back out of state, so the pristine
    // fingerprint below is taken from the very values this open hydrates with
    // (issue #1006). Reading state here would fingerprint the PREVIOUS open —
    // React has not applied these setters yet — and every field would then
    // count as edited.
    let nextId = workflow?.id ?? "";
    let nextName = workflow?.name ?? "";
    let nextDescription = workflow?.description ?? "";
    // No control edits this (see `ownerDesk` state above) — hydrated purely so
    // Save round-trips it. `undefined` on a fresh create, same as every other
    // field here.
    let nextOwnerDesk = workflow?.ownerDesk;
    let nextNodes = workflow ? draftNodes(workflow) : starterNodes();
    let nextEdges = workflow ? draftEdges(workflow) : [];
    setError(null);
    setFieldErrors({});
    // Issue #1808: a fresh open (or a re-hydrate) never carries a prior attempt's
    // pending id confirm — the previewed id it named may not be this graph's.
    setConfirmingId(false);
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
      // Issue #1053: chosen by the copilot, not left blank — editing the name
      // afterwards must not slug over it. Only the latch is written here; the id
      // itself rides `nextId` like every other hydrated field, so the pristine
      // fingerprint below is taken from the value this open actually applies
      // (issue #1006) rather than from state React has not committed yet.
      setIdTouched(true);
      nextId = workflow?.id ?? g.id;
      nextName = g.name.trim();
      nextDescription = g.description ?? "";
      nextNodes = draftNodes(g);
      nextEdges = draftEdges(g);
      setDraftSummary(prefilledDraft.summary ?? null);
      setDraftNotes((prefilledDraft.notes ?? []).filter((n) => n.trim()));
      setReadiness(prefilledDraft.readiness ?? null);
    } else {
      setReadiness(null);
    }
    setId(nextId);
    setName(nextName);
    setDescription(nextDescription);
    setOwnerDesk(nextOwnerDesk);
    setNodes(nextNodes);
    setEdges(nextEdges);
    // The baseline every later keystroke is compared against. A copilot
    // correction counts as pristine for the same reason a saved graph does:
    // the operator has not touched it yet, so closing loses nothing they wrote.
    pristineRef.current = draftFingerprint(
      nextId,
      nextName,
      nextDescription,
      nextNodes,
      nextEdges,
    );
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
    //
    // #981: the two outcomes are recorded as DIFFERENT states. A settled answer
    // is knowledge the pre-flight checks against (even when it is `[]`); a
    // failure is not, and must never be mistaken for one. Reset to `loading` on
    // every open so a reopened dialog re-asks rather than validating against the
    // previous company's answer.
    setWiredChannels({ status: "loading" });
    (async () => {
      try {
        const channels = await listWiredChannels(client, company);
        if (live) setWiredChannels({ status: "ready", ids: channels });
      } catch {
        if (live) setWiredChannels({ status: "unavailable" });
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
   * Whether the form holds edits that closing would destroy (issue #1006).
   *
   * Recomputed every render rather than memoised: the fingerprint is a few
   * hundred bytes of `JSON.stringify` over a form that already re-renders on
   * every keystroke, and a memo keyed on the draft can go stale against
   * `pristineRef` on the render where a re-hydrate lands the same values it
   * replaced — reporting unsaved work that no longer exists.
   */
  const dirty =
    open && draftFingerprint(id, name, description, nodes, edges) !== pristineRef.current;

  /** Close the dialog, asking first if that would throw work away (#1006).
   * Every deliberate exit routes through here — Esc, a click outside, Cancel —
   * so there is one answer to "does this lose my edits?" rather than three. */
  const requestClose = useCallback(() => {
    if (dirty && !window.confirm(DISCARD_PROMPT)) return;
    onOpenChange(false);
  }, [dirty, onOpenChange]);

  // Issue #1006: the tab-level guard. A reload or a close is the one exit the
  // dialog cannot intercept itself, so it hands the question to the browser.
  useEffect(() => {
    if (!dirty) return;
    const onBeforeUnload = (e: BeforeUnloadEvent) => {
      // `preventDefault` is what modern browsers read; `returnValue` is the
      // legacy spelling some still require. Neither shows our text — the
      // browser substitutes its own — so there is nothing to word here.
      e.preventDefault();
      e.returnValue = "";
    };
    window.addEventListener("beforeunload", onBeforeUnload);
    return () => window.removeEventListener("beforeunload", onBeforeUnload);
  }, [dirty]);

  // Issue #1006: the in-app guard. The console routes on the hash, so Back and
  // every sidebar link arrive here — including the Back press that used to walk
  // the selection onto a DIFFERENT workflow and re-hydrate this dialog with it
  // mid-edit. `hashchange` fires after the address bar has already moved, so
  // declining puts it back; the restore fires the event again, which the
  // equality check below absorbs.
  useEffect(() => {
    if (!dirty) return;
    let at = window.location.hash;
    const onHashChange = () => {
      const moved = window.location.hash;
      if (moved === at) return;
      if (window.confirm(DISCARD_PROMPT)) {
        at = moved;
        onOpenChange(false);
        return;
      }
      window.location.hash = at;
    };
    window.addEventListener("hashchange", onHashChange);
    return () => window.removeEventListener("hashchange", onHashChange);
  }, [dirty, onOpenChange]);

  // The graph the pre-flight would ask about, and the key that identifies it.
  // `null` when there is nothing worth asking: the client checks already have a
  // complaint (no point spending a round trip to be told something the operator
  // is about to fix), or the config serializer disagreed with the form.
  const assembledForPreflight = open && !submitting && !validate() ? assembleGraph({
    id,
    name,
    description,
    ownerDesk,
    nodes,
    edges,
  }) : null;
  const preflightKey =
    assembledForPreflight?.ok === true ? JSON.stringify(assembledForPreflight.graph) : null;

  /**
   * Asks the host whether Create would accept the graph on screen (issue #1074).
   *
   * This is the point of the validate route. Two of Create's rules — node
   * reachability and the condition branch-label rule — cannot be pre-empted by
   * this dialog without re-implementing them, and a client-side copy of a host
   * rule drifts. So the dialog asks rather than mirrors, and it asks BEFORE the
   * operator presses Create: pre-empting a refusal is the whole subject of the
   * issue, and asking at submit time would cost a round trip to learn what the
   * submit's own error already says.
   *
   * Debounced, and keyed on the serialized body. The key is what makes a verdict
   * discardable: a response for a graph the author has already changed is not an
   * answer about the graph on screen, and showing it would be the false green
   * #1048 was about one layer up. `preflightIsCurrent` gates the render on the
   * same key, so a superseded response cannot be displayed even if it lands.
   *
   * It never blocks Create. The host decides at submit, this only reports early —
   * so a host that has never heard of the route (or an offline console) degrades
   * to `unavailable` and the dialog behaves exactly as it did before.
   */
  useEffect(() => {
    if (!preflightKey) {
      setPreflight({ status: "idle" });
      return;
    }
    let live = true;
    const timer = setTimeout(() => {
      setPreflight({ status: "asking", key: preflightKey });
      validateWorkflow(client, company, JSON.parse(preflightKey) as WorkflowGraph)
        .then(() => {
          if (live) setPreflight({ status: "ok", key: preflightKey });
        })
        .catch((e: unknown) => {
          if (!live) return;
          // A 400 IS the answer — the host's own words, the same body Create
          // would have sent. Anything else (offline, 404 on an older host, a
          // 5xx) is a failure to ASK, which is not a verdict on the graph and
          // must never be shown as one.
          if (e instanceof ApiError && e.status === 400) {
            setPreflight({ status: "refused", key: preflightKey, message: e.message });
          } else {
            setPreflight({ status: "unavailable", key: preflightKey });
          }
        });
    }, PREFLIGHT_DEBOUNCE_MS);
    return () => {
      live = false;
      clearTimeout(timer);
    };
  }, [preflightKey, client, company]);

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
    // Issue #1808: the name derives the previewed id, so editing it after a
    // Back invalidates whatever the confirm was showing — retire the pending
    // confirm so a stale preview can't reappear on the next Create.
    setConfirmingId(false);
    // Issue #1053: the form used to reject "Weekly digest" for a missing id,
    // then reject "weekly digest" for an unsafe one — twice, for something it
    // could derive. Derived only while the id is nobody's yet, and never in edit
    // mode, where the id keys the saved graph and re-slugging it is a rename.
    if (editing || idTouched) return;
    const derived = slugifyWorkflowId(value);
    // An empty derivation means the name had nothing usable in it. Leave the
    // field alone rather than writing "" — an empty id is itself invalid, and
    // blanking a good id on a bad keystroke is the clobber this guard prevents.
    if (derived) setId(derived);
  }

  /** Same for the id, which is what `validate()` complains about FIRST (missing,
   * or not a safe id) and the most common 409 from the host — so it is the
   * banner an author is most often looking at while fixing its cause. */
  function changeId(value: string) {
    // Issue #1053: the operator has taken the field — the name stops writing it,
    // including when they clear it back to empty, which is a decision too.
    setAuthoredId(value);
    clearSubmitError();
    // Issue #1808: the id the confirm would show just changed under it — retire
    // the pending confirm so Back-then-edit re-derives the new one.
    setConfirmingId(false);
  }

  function changeDescription(value: string) {
    setDescription(value);
    clearSubmitError();
  }

  function updateNode(key: string, fields: Partial<DraftNode>) {
    // The row's id BEFORE this edit, read off the render snapshot the same way
    // `removeNode` does. An edge references a node by its id, so a rename has to
    // carry every edge that pointed at the old id over to the new one (issue
    // #1016) — otherwise the edge dangles, `validate()` refuses the save, and
    // the edge Select's option list (fed by the current node ids) drops the
    // renamed node, leaving the operator nothing to re-point it at.
    const prevId = nodes.find((n) => n.key === key)?.id;
    setNodes((rows) => rows.map((r) => (r.key === key ? { ...r, ...fields } : r)));
    const nextId = fields.id;
    // `""` is a real previous id (the field cleared mid-edit, see below) and
    // must still be tracked — only a missing row (`prevId === undefined`) or
    // an edit that didn't touch `id` (`nextId === undefined`) skips the
    // cascade. Using `prevId &&`/`fields.id` truthiness here previously
    // dropped the rewrite once `prevId` was `""`, so clearing an id and then
    // typing a replacement left edges stranded pointing at `""` forever.
    if (nextId !== undefined && prevId !== undefined && nextId !== prevId) {
      // Continuously, on every keystroke of the id: the edges track the row's
      // id so a rename can never orphan them. A transient empty id (the field
      // cleared mid-edit) cascades to `""`, which is harmless — `validate()`
      // blocks the save on it and the edges re-follow on the next keystroke.
      setEdges((rows) =>
        rows.map((e) => ({
          ...e,
          from: e.from === prevId ? nextId : e.from,
          to: e.to === prevId ? nextId : e.to,
        })),
      );
    }
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
      problem = destinationTargetProblem(
        node.destinationKind,
        value,
        wiredChannels,
      );
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

  /**
   * Connect each visible node to the next one, preserving explicit branches and
   * labels already authored. Existing pairs count as connected regardless of
   * label, so pressing the affordance twice never adds duplicate edges.
   */
  function connectNodesInOrder() {
    setEdges((rows) => edgesConnectingNodesInOrder(nodes, rows));
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
      //
      // #981: ask the deferral FIRST. While the wired-channel list is in
      // flight the target check below cannot answer, and its `null` would read
      // as a pass — which made the strength of the pre-flight depend on how
      // fast the author clicked Save.
      const deferred = destinationCheckDeferred(
        n.destinationKind,
        n.destinationTarget,
        wiredChannels,
      );
      if (deferred) return `Node \`${nodeLabel(n)}\`: ${deferred}`;
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

  // Issue #1052: refresh the post-await view of dirtiness on every render. See
  // `draftDirtyRef` for why re-calling `isDraftDirty()` after an await cannot
  // work on its own.
  useEffect(() => {
    draftDirtyRef.current = isDraftDirty();
  });

  // Issue #1016: keep the post-await view of the node rows current, so `submit`'s
  // catch matches the host's `problems` against what is on screen when the answer
  // lands — not the snapshot its closure captured at click time.
  useEffect(() => {
    nodesRef.current = nodes;
  });

  /** Draft a graph from the description and hydrate the form with it (issue
   * #753). The hydrated, editable form IS the review surface — there is no
   * read-only diff — so on success the operator lands in the ordinary create
   * form with everything filled in, tweaks if needed, and presses Create. */
  async function runDraft() {
    const description = copilotPrompt.trim();
    if (!description || drafting || echoing) return;
    // Issue #1052: the consent for overwriting the operator's work is taken
    // AFTER the await, not here. A model call takes seconds and the operator is
    // invited to keep typing through it, so a confirm asked now would be
    // answered about a form that no longer exists when the draft lands — and
    // the answer would then authorise replacing work started after it was
    // given. It also asked at all for a draft that turns out not automatable,
    // which leaves the form untouched and needed no permission.
    const requestedEpoch = draftEpochRef.current;
    setDrafting(true);
    setDraftError(null);
    setDraftSummary(null);
    setDraftReason(null);
    setDraftNotes([]);
    try {
      const drafted = await draftWorkflowFromDescription(client, company, description);
      // Issue #1052: what this response is allowed to do to the form it came
      // back to, decided against the form as it is NOW.
      const landing = draftLanding({
        requestedEpoch,
        currentEpoch: draftEpochRef.current,
        dirtyNow: draftDirtyRef.current,
      });
      // The dialog moved on — closed, reopened, re-hydrated. Drop it silently:
      // nothing was asked of the operator, so nothing needs explaining, and the
      // banners below belong to a form that is gone.
      if (landing === "drop") return;
      const banners = draftBanners(drafted);
      if (drafted.automatable && drafted.workflow) {
        // Only a draft that will actually replace something asks. `confirm`
        // means the form holds work right now; declining leaves it exactly as
        // the operator left it, prompt and all, so they can draft again.
        if (
          landing === "confirm" &&
          !window.confirm(
            "Replace what you've started with the drafted workflow? You can still edit it before creating.",
          )
        ) {
          return;
        }
        const graph = drafted.workflow;
        // Hydrate via the same helpers edit mode uses, so a drafted graph and a
        // saved one populate the form identically.
        // Issue #1053: the copilot chose this id, so the name stops writing it.
        setAuthoredId(graph.id);
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
      // Issue #1052: only the request that owns the current contents may clear
      // the spinner — a stale one would switch off a draft the operator is
      // actually waiting on.
      if (draftEpochRef.current === requestedEpoch) setDrafting(false);
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

  /**
   * Assembles the graph and runs one write, carrying the submit-time guard,
   * the spinner state, and the host-error handling (issue #1005/#1016).
   *
   * Both Create and Save go through here so there is a SINGLE write path: the
   * re-entrancy guard, the `workflow_invalid` per-node mapping, and the 409
   * conflict handoff live once. The caller supplies only the verb — `create()`
   * posts, the edit branch of `submit()` puts — via `write`.
   */
  async function runWrite(write: (graph: WorkflowGraph) => Promise<void>) {
    // Set before the first `await` — the caller has already run `validate()`, so
    // a draft the client rejects never latches the guard.
    submittingRef.current = true;
    setSubmitting(true);
    setError(null);
    try {
      // Issue #1006: the graph is assembled INSIDE the `try`. It used to be
      // built above it, so a serialisation failure escaped past the `finally`
      // and left `submitting` stuck true — which disables Cancel and gates the
      // dialog's own `onOpenChange`. The operator was locked in the dialog, and
      // the only remaining exit, reloading the page, was the one that lost the
      // edit. Everything that can fail now clears `submitting` on the way out.
      const assembled = assembleGraph({ id, name, description, ownerDesk, nodes, edges });
      if (!assembled.ok) {
        // `validate()` already passed, so this is the form and the serializer
        // disagreeing — a defect, not something the author did. Say which node
        // it was and leave the draft exactly as it is: the dialog stays open,
        // closable, with the work still in it.
        showError(`${nodeLabel(assembled.node)}: ${assembled.error}`);
        return;
      }
      await write(assembled.graph);
      onOpenChange(false);
    } catch (e) {
      // Issue #1016: a `workflow_invalid` refusal carries per-node `problems`.
      // Land each on the control that caused it so the operator sees the
      // complaint next to the field, instead of a flat banner that names a node
      // they then have to hunt for. Anything without an on-screen home — a
      // graph-level field (`from`/`to`/`workflow_id`), a config key this kind
      // has no control for, or a node that no longer exists — falls through to
      // the banner, so nothing the host said is ever silently dropped.
      if (e instanceof ApiError && e.problems?.length) {
        const mapped: Record<string, string> = {};
        const leftovers: string[] = [];
        for (const p of e.problems) {
          // Matched against the CURRENT rows (`nodesRef`), not the closure's
          // snapshot: the operator may have renamed a node during the write, and
          // a stale `node_id` must fall back to the banner rather than misfile.
          // `.trim()` on our side because the submit path trims every id before
          // sending it (see `outNodes.push` above) — the host's `problems`
          // therefore carry the trimmed id, and comparing it against a raw
          // draft id with surrounding whitespace would never match.
          const row = nodesRef.current.find((n) => n.id.trim() === p.node_id);
          const configKey = p.field?.startsWith("config.")
            ? p.field.slice("config.".length)
            : undefined;
          const onScreen =
            row !== undefined &&
            configKey !== undefined &&
            configFieldSpecs(row.kind).some((s) => s.key === configKey);
          if (row && onScreen) {
            mapped[errorKey(row.key, `config:${configKey}`)] = p.message;
          } else {
            leftovers.push(row ? `${nodeLabel(row)}: ${p.message}` : p.message);
          }
        }
        // One write, merged over any blur errors (#261) already showing — a
        // server field-error clears on the next edit of that field or the next
        // submit, never wiping a legitimate blur error the operator has not
        // touched.
        if (Object.keys(mapped).length) {
          setFieldErrors((prev) => ({ ...prev, ...mapped }));
        }
        // Everything that had no field home goes to the banner. If it ALL landed
        // on a field, the banner still says something non-raw so Create never
        // reads as a button that did nothing.
        showError(
          leftovers.length
            ? leftovers.join(" ")
            : "Some fields need attention — see the highlighted nodes below.",
        );
        return;
      }
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
      // Issue #1808: the create-mode confirm steps aside on every terminal
      // state. Success closes the whole dialog above; a failure surfaces the
      // banner on the form the confirm was covering — either way the modal must
      // not linger, or its inert backdrop swallows the next click. A no-op in
      // edit mode, where `confirmingId` is never set.
      setConfirmingId(false);
    }
  }

  /**
   * The create write, gated behind the id confirm (issue #1808). The confirm's
   * primary action calls this; the shared guard in {@link runWrite} keeps it
   * single-fire even though it is reachable only after the confirm opens.
   */
  async function create() {
    if (submittingRef.current) return;
    await runWrite(async (graph) => {
      const created = await createWorkflow(client, company, graph);
      onCreated?.(created);
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
    // both see `false` and the guard would pass twice.
    if (submittingRef.current) return;
    const problem = validate();
    if (problem) {
      showError(problem);
      return;
    }
    // Issue #1808: create mode confirms the permanent id before it writes. The
    // previewed id is valid by here (validate() passed), so the confirm shows a
    // real id, and the confirm's own action runs `create()`. Edit mode falls
    // straight through — the id keys the saved graph and the field is read-only,
    // so there is nothing to confirm.
    if (!editing) {
      if (!confirmingId) {
        setConfirmingId(true);
        return;
      }
      await create();
      return;
    }
    await runWrite(async (graph) => {
      // The id keys the saved graph, the schedule and the run history, so it is
      // the graph's own id that is sent, not the (read-only) field — there is no
      // path here that renames anything, and the host answers 400 if one ever
      // appeared. `version` makes the write conditional: "save over the graph I
      // was looking at", not "over whatever is there now". The response carries a
      // fresh token, so a second save needs no intervening read.
      const saved = await updateWorkflow(
        client,
        company,
        workflow!.id,
        graph,
        workflow!.version,
      );
      onSaved?.(saved);
    });
  }

  return (
    <>
    <Dialog
      open={open}
      onOpenChange={(o) => {
        if (submitting) return;
        // Issue #1006: Base UI reports Esc and an outside click through here,
        // and both used to close unconditionally. Route them through the same
        // confirm as Cancel so no exit is quieter than the others.
        if (o) onOpenChange(true);
        else requestClose();
      }}
    >
      {/* `aria-busy` while a save is in flight (issue #1005): the dialog stays
          on screen and mostly interactive during the round trip, so without it
          a screen reader has nothing to say the form is mid-write. */}
      <DialogContent
        className="max-h-[85vh] overflow-y-auto sm:max-w-2xl"
        aria-busy={submitting}
      >
        <DialogHeader>
          {/* Issue #1006: an edit names the workflow it is editing. The dialog
              is reachable from a canvas, a card and a run, and said "Edit
              workflow" from all of them — so an operator whose selection had
              moved underneath them had nothing on screen to notice it with. */}
          <DialogTitle>
            {editing
              ? `Edit “${workflow?.name?.trim() || workflow?.id}”`
              : "New workflow"}
          </DialogTitle>
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
              <Label htmlFor={`${formId}-name`}>Name</Label>
              <Input
                id={`${formId}-name`}
                value={name}
                onChange={(e) => changeName(e.target.value)}
                placeholder="e.g. Campaign pipeline"
              />
            </div>
            <div className="grid gap-2">
              <Label htmlFor={`${formId}-id`}>Workflow ID</Label>
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
              <p className="text-2xs leading-snug text-muted-foreground">
                {editing
                  ? "This permanent machine ID can’t change. It keys the saved graph, its schedule and its run history."
                  : "Generated from the name. You can change it now; after creation it becomes the permanent machine ID for schedules and run history."}
              </p>
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
            <div className="flex flex-wrap items-center justify-between gap-2">
              <div>
                <Label>Connections</Label>
                <p className="text-2xs text-muted-foreground">
                  Connect a simple sequence automatically, or edit branches explicitly.
                </p>
              </div>
              <div className="flex items-center gap-2">
                <Button
                  type="button"
                  variant="outline"
                  size="sm"
                  onClick={connectNodesInOrder}
                  disabled={
                    submitting ||
                    nodes.length < 2 ||
                    nodes.some((node) => !node.id.trim()) ||
                    new Set(nodes.map((node) => node.id.trim())).size !== nodes.length
                  }
                >
                  Connect in order
                </Button>
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
            </div>
            <div className="space-y-2">
              {edges.map((e) => (
                <EdgeRow
                  key={e.key}
                  edge={e}
                  nodes={nodes
                    .map((n) => ({
                      id: n.id.trim(),
                      kind: n.kind,
                      onError: n.onError,
                    }))
                    .filter((n) => n.id)}
                  onChange={(fields) => updateEdge(e.key, fields)}
                  onRemove={() => removeEdge(e.key)}
                />
              ))}
              {edges.length === 0 && (
                <p className="rounded-lg border border-dashed p-3 text-center text-xs text-muted-foreground">
                  No connections yet — connect the steps in order or add an explicit edge.
                </p>
              )}
            </div>
          </div>
        </fieldset>

        {/* The host's early verdict (issue #1074). Rendered only while it still
            describes the graph on screen, and only when the submit-time banner
            is not already up — `error` is the answer to something the operator
            just asked for, and it outranks an advisory. Deliberately NOT
            `assertive` and it does not move focus: nobody asked for it, and an
            author mid-edit must not be interrupted by it. `unavailable` and
            `asking` render nothing at all: "we could not ask" is not a verdict
            on the graph, and a spinner on every pause is noise. */}
        {!error && preflightIsCurrent(preflight, preflightKey ?? "") && (
          <>
            {preflight.status === "refused" && (
              <div role="status" aria-live="polite" data-testid="preflight-refused">
                {/* `Alert` defaults to `role="alert"` (assertive). Nobody asked
                    for this verdict, so it must not interrupt an edit. */}
                <Alert variant="destructive" role="status">
                  <AlertDescription>
                    The host would refuse this graph: {preflight.message}
                  </AlertDescription>
                </Alert>
              </div>
            )}
            {preflight.status === "ok" && (
              <p
                role="status"
                aria-live="polite"
                data-testid="preflight-ok"
                className="text-xs text-muted-foreground"
              >
                Checked with the host: this graph would be accepted. A name or id
                already taken is still only decided when you save.
              </p>
            )}
          </>
        )}

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
          <Button variant="ghost" onClick={requestClose} disabled={submitting}>
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
      {/* Issue #1808: the create-time id confirm. The id is a permanent backend
          join key set only at creation and silently derived from the name, so a
          typo becomes a permanent id with no acknowledgement — this is the one
          moment to surface it. Create mode only; an edit has no id to set. An
          AlertDialog (focus-trapped, labelled, matching the console) rather than
          `window.confirm`, and it surfaces the exact id the write will send. */}
      {!editing && (
        <AlertDialog
          open={confirmingId}
          onOpenChange={(o) => {
            // Opening is driven by `submit()`; only react to a dismiss — Esc, an
            // outside click, or the Close primitive behind Back/Create.
            if (!o) setConfirmingId(false);
          }}
        >
          <AlertDialogContent data-testid="workflow-id-confirm">
            <AlertDialogHeader>
              <AlertDialogTitle>Confirm the workflow ID</AlertDialogTitle>
              <AlertDialogDescription>
                The ID is permanent — it keys this workflow’s schedule and run
                history and can’t be changed after creation. Check it now.
              </AlertDialogDescription>
            </AlertDialogHeader>
            <div className="grid gap-1 rounded-md border bg-muted/40 p-3 text-center">
              {name.trim() && (
                <span className="text-xs text-muted-foreground">{name.trim()}</span>
              )}
              <code
                data-testid="workflow-id-confirm-value"
                className="font-mono text-lg font-semibold break-all"
              >
                {id.trim()}
              </code>
            </div>
            <AlertDialogFooter>
              <AlertDialogCancel
                data-testid="workflow-id-confirm-back"
                onClick={() => setConfirmingId(false)}
                disabled={submitting}
              >
                Back
              </AlertDialogCancel>
              {/* Fire-and-forget, the repo idiom for an async confirm action:
                  our handler runs before the primitive's Close, so the write is
                  launched and the confirm dismisses in the same click. */}
              <AlertDialogAction
                data-testid="workflow-id-confirm-create"
                onClick={() => void create()}
                disabled={submitting}
              >
                {submitting && <Loader2 className="mr-1.5 size-4 animate-spin" />}
                Create workflow
              </AlertDialogAction>
            </AlertDialogFooter>
          </AlertDialogContent>
        </AlertDialog>
      )}
    </>
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
   * picker's options. Anything but a settled, non-empty answer degrades the
   * channel target to a free-text box (#981). */
  wiredChannels: WiredChannels;
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
  // The channels there are to offer, which is none until the host has answered
  // (#981). A picker with no options is worse than the free-text fallback, so
  // this drives both which control renders and which explanation sits under it.
  const channelIds = channelOptions(wiredChannels);
  return (
    <div className="grid gap-2 rounded-lg border p-2 sm:grid-cols-[1fr_1fr_1.4fr_auto] sm:items-start">
      <div className="grid gap-1">
        <Label htmlFor={`${rowId}-id`} className="text-2xs text-muted-foreground">
          Node ID
        </Label>
        <Input
          id={`${rowId}-id`}
          value={node.id}
          onChange={(e) => onChange({ id: e.target.value })}
          placeholder="node id"
          aria-label="Node id"
        />
        {/* Changing the kind clears every kind-conditional field, so the draft
            never holds a value whose control is no longer on screen. Without
            this, picking a destination and then changing the kind left the row
            un-submittable with nothing visible to clear. */}
        <Label className="mt-1 text-2xs text-muted-foreground">Kind</Label>
        <Select value={node.kind} onValueChange={(v) => onChange(changeKind(v ?? ""))}>
          <SelectTrigger className="h-8" aria-label="Node kind">
            <SelectValue />
          </SelectTrigger>
          <SelectContent>
            {NODE_KINDS.map((k) => (
              <SelectItem key={k.value} value={k.value}>
                {k.label}
              </SelectItem>
            ))}
          </SelectContent>
        </Select>
      </div>
      <div className="grid gap-1">
        <Label htmlFor={`${rowId}-name`} className="text-2xs text-muted-foreground">
          Step name
        </Label>
        <Input
          id={`${rowId}-name`}
          value={node.name}
          onChange={(e) => onChange({ name: e.target.value })}
          placeholder="display name"
          aria-label="Node name"
        />
        {node.kind === "agent" &&
          (roster.length > 0 ? (
            <>
              <Label className="mt-1 text-2xs text-muted-foreground">Teammate</Label>
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
            </>
          ) : (
            <>
              <Label htmlFor={`${rowId}-teammate`} className="mt-1 text-2xs text-muted-foreground">
                Teammate ID
              </Label>
              <Input
                id={`${rowId}-teammate`}
                value={node.agent}
                onChange={(e) => onChange({ agent: e.target.value })}
                placeholder="teammate id"
                aria-label="Teammate id"
              />
            </>
          ))}
      </div>
      <div className="grid gap-1">
        <Label htmlFor={`${rowId}-summary`} className="text-2xs text-muted-foreground">
          Summary
        </Label>
        <Input
          id={`${rowId}-summary`}
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
            {node.destinationKind === "channel" && channelIds.length > 0 && (
              <>
                {/* #813: pick from the channels this company can actually
                    deliver to, instead of a free-text box that only fails at
                    delivery time. #1757: the list now includes `operator` —
                    it is a durable, journal-backed channel every company
                    wires, not the in-memory surface #981 once excluded — so
                    the picker offers it like any other real target. */}
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
                    {channelIds.map((channelId) => (
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
              (node.destinationKind === "channel" && channelIds.length === 0)) && (
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
                Needs this company to grant email — the save is refused
                otherwise — and the recipient to have already written in.
              </p>
            )}
            {/* #981: an empty list is now a legitimate answer — a company with
                no desks and no connected channels has nowhere to post a report,
                so the free-text box above is the honest fallback rather than a
                picker of one bad option. Say why it is empty; without this the
                author sees a blank box and no reason.

                Gated on `ready`: while the request is in flight, or when it
                failed, the box is equally empty and this sentence would be a
                claim we cannot make. Each of those states says its own thing
                below instead. */}
            {node.destinationKind === "channel" &&
              wiredChannels.status === "ready" &&
              wiredChannels.ids.length === 0 && (
                <p className="text-2xs leading-snug text-muted-foreground">
                  No delivery channels are wired for this company — add a desk,
                  or connect a channel, before a report can be posted to one.
                </p>
              )}
            {node.destinationKind === "channel" && wiredChannels.status === "loading" && (
              <p className="text-2xs leading-snug text-muted-foreground">
                Checking which channels this company can deliver to…
              </p>
            )}
            {node.destinationKind === "channel" &&
              wiredChannels.status === "unavailable" && (
                <p className="text-2xs leading-snug text-muted-foreground">
                  This host did not say which channels it can deliver to, so the
                  target is checked when the workflow is saved.
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
  nodes,
  onChange,
  onRemove,
}: {
  edge: DraftEdge;
  /** Every node the edge may point at. Rows rather than bare ids (issue #1074):
   * the label control depends on the SOURCE node's `kind` and `onError`, and an
   * id alone cannot answer either. */
  nodes: EdgeEndpoint[];
  onChange: (fields: Partial<DraftEdge>) => void;
  onRemove: () => void;
}) {
  // Stable across renders, so `aria-describedby` keeps pointing at the same
  // element as the operator edits the row.
  const problemId = useId();
  const source = nodes.find((n) => n.id === edge.from);
  // `null` = not a condition, so any label is legal and the row keeps its
  // free-text input. Recomputed from `from` on every render, so re-pointing an
  // edge at (or away from) a condition swaps the control immediately — and
  // never rewrites the label it finds there.
  const branch = conditionBranchChoice(source, edge.label);
  return (
    <div className="grid grid-cols-[1fr_auto_1fr_1fr_auto] items-center gap-2 rounded-lg border p-2">
      <Select value={edge.from} onValueChange={(v) => onChange({ from: v ?? "" })}>
        <SelectTrigger className="h-8" aria-label="Edge from">
          <SelectValue placeholder="from" />
        </SelectTrigger>
        <SelectContent>
          {nodes.map((n) => (
            <SelectItem key={n.id} value={n.id}>
              {n.id}
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
          {nodes.map((n) => (
            <SelectItem key={n.id} value={n.id}>
              {n.id}
            </SelectItem>
          ))}
        </SelectContent>
      </Select>
      {branch ? (
        <div className="space-y-1">
          <Select value={branch.value} onValueChange={(v) => onChange({ label: v ?? "" })}>
            <SelectTrigger
              className="h-8"
              aria-label="Edge label"
              // The problem below is the only thing that says WHY this row is
              // wrong, and a sighted author reads it off the red text under the
              // control. Without these it is invisible to assistive tech.
              aria-invalid={branch.problem ? true : undefined}
              aria-describedby={branch.problem ? problemId : undefined}
            >
              <SelectValue placeholder="branch" />
            </SelectTrigger>
            <SelectContent>
              {branch.options.map((option) => (
                <SelectItem key={option} value={option}>
                  {option}
                </SelectItem>
              ))}
              {/* A saved graph can carry a label this rule does not accept, and
                  a Select shows nothing for a value it has no item for. Carrying
                  it as an item is what makes the operator SEE what is there
                  instead of watching it vanish. */}
              {branch.value && !branch.options.includes(branch.value) && (
                <SelectItem value={branch.value}>{branch.value}</SelectItem>
              )}
            </SelectContent>
          </Select>
          {branch.problem && (
            <p id={problemId} className="text-xs text-destructive">
              {branch.problem}
            </p>
          )}
        </div>
      ) : (
        <Input
          value={edge.label}
          onChange={(e) => onChange({ label: e.target.value })}
          placeholder="label (optional)"
          aria-label="Edge label"
        />
      )}
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
