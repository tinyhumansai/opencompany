// The live workflow API: the console's Workflows canvas reads the company's
// saved graphs through the host's `…/workflows` routes (REST, camelCase over
// the wire) and runs one via `…/workflows/{wid}/run`. Replaces the client-side
// `workflow-sample` illustrative data.

import type { OpenCompanyClient } from "./client";

/** A one-line workflow entry, as the picker lists it. */
export interface WorkflowSummary {
  id: string;
  name: string;
  description?: string;
  /**
   * Whether this workflow can be edited or deleted through the API (issue
   * #259). `false` for a graph defined by a file in the company source tree,
   * and for a name-only entry with no saved graph at all — the host refuses
   * both with a 409, so the console greys the affordance out instead.
   *
   * **Optional on the type, not on the wire.** A host predating #259 sends no
   * such field, and `undefined` must not read as "not editable" — treat only an
   * explicit `false` as a refusal.
   */
  editable?: boolean;
}

/** A single graph node. `kind` is one of the tinyflows node kinds. */
export interface WorkflowNode {
  id: string;
  /**
   * `trigger` | `agent` | `tool_call` | `http_request` | `condition` |
   * `output` | `switch` | `merge` | `split_out` | `transform` |
   * `output_parser` | `sub_workflow`.
   */
  kind: string;
  name: string;
  summary?: string;
  /** The roster agent id — only present on `agent` nodes. */
  agent?: string;
  /**
   * A standard 5-field cron saying when the workflow starts on its own — only
   * present on `trigger` nodes, and always interpreted in **UTC** (issue #169).
   * Absent means the workflow only runs when something starts it (the Run
   * button, the run route, or another workflow).
   */
  schedule?: string;
  /** Kind-specific configuration (a slug, URL, case labels, schema, …). */
  config?: unknown;
  /** How the engine handles an error on this node, when set. */
  onError?: string;
  /** The node's retry policy, when set. */
  retry?: {
    maxAttempts?: number;
    backoffMs?: number;
    backoff?: string;
  };
  /** Whether the node pauses for a human approval before proceeding. */
  requiresApproval?: boolean;
  /** Where an `output` node's report goes when the run finishes. */
  destination?: WorkflowDestination;
}

/**
 * Where a terminal `output` node routes its report once the run completes.
 *
 * `owner` is resolved server-side from the company's admins and carries no
 * target — the graph names nobody, which is what keeps it safe by construction.
 * `email` names an address and only sends when the company grants `email` AND
 * the recipient has already written in. `channel` must name a channel the
 * deployment already wired.
 */
export interface WorkflowDestination {
  kind: "owner" | "email" | "channel";
  /** Required for `email` (an address) and `channel` (an id); absent for `owner`. */
  target?: string;
}

/**
 * The destination kinds the creator's picker offers, with prosumer labels.
 *
 * Kept in step with the host's `WORKFLOW_DESTINATION_KINDS` by
 * `destination_kinds_match_the_console` in `src/company/workflow_file.rs`, which
 * reads the `value:` entries out of this very block — so adding a kind on one
 * side alone fails `cargo test` rather than shipping a picker the server rejects
 * (or withholding one it accepts). Issue #260.
 */
export const DESTINATION_KINDS: { value: WorkflowDestination["kind"]; label: string }[] = [
  { value: "owner", label: "Owner — the company's admins" },
  { value: "email", label: "Email — a specific address" },
  { value: "channel", label: "Channel — a wired chat channel" },
];

/** A directed edge between two node ids, with an optional branch label. */
export interface WorkflowEdge {
  from: string;
  to: string;
  label?: string;
}

/** The full graph the canvas renders. */
export interface WorkflowGraph {
  id: string;
  name: string;
  description?: string;
  nodes: WorkflowNode[];
  edges: WorkflowEdge[];
  /** See {@link WorkflowSummary.editable}. Same "only `false` means no" rule. */
  editable?: boolean;
  /**
   * The opaque optimistic-concurrency token for this graph (issue #259),
   * present only when `editable`.
   *
   * **Echo it back, never parse it.** Pass it to {@link updateWorkflow} or
   * {@link deleteWorkflow} and the host refuses the write with a 409 if the
   * graph changed since this read — which is what stops one console silently
   * overwriting another's edit. Absent from a host predating #259, in which case
   * the write is unconditional and that protection simply does not exist.
   */
  version?: string;
}

/**
 * What became of one attempt to deliver an output node's report.
 *
 * `pending` means the send was parked for operator approval (a cold email
 * recipient a workflow may not open a conversation with on its own). It is a
 * SNAPSHOT taken when the run finished: runs are not persisted, so nothing ever
 * comes back to flip the row to `sent`. The Approvals view is the live source of
 * truth — approving there actually sends the mail.
 */
export type DeliveryStatus = "sent" | "pending" | "skipped" | "denied" | "failed";

/**
 * One attempt to route a reached `output` node's report to its destination.
 *
 * This is the ONLY place an operator learns a report was not delivered: a
 * delivery failure never fails the run, so it has nowhere else to surface. An
 * output node the run never reached contributes no row at all.
 */
export interface DeliveryReport {
  /** The output node whose report this was. */
  node: string;
  /** The destination kind as authored. */
  kind: string;
  /** The address or channel actually addressed, when there was one. */
  target?: string;
  status: DeliveryStatus;
  /**
   * An operator-readable reason — populated even on success. This is the half
   * the drawer renders: it may quote the transport verbatim, which is what
   * makes a refused send fixable, and this console is a tenant surface.
   */
  detail: string;
  /**
   * The same outcome as a stable token out of a closed set (issue #248) —
   * `"mail-transport-refused"`, `"channel-not-wired"`, and so on.
   *
   * It exists because the host's own logs may not carry `detail`: on the
   * transport-failure arms `detail` interpolates the transport's reply, which
   * routinely quotes the recipient's address, and host stdout is a platform
   * surface rather than a tenant one. Nothing renders it today — `detail` is
   * strictly more informative here — so it is optional on the type (not the
   * wire), and a response from a host predating #248 still parses.
   */
  reason?: string;
}

/** The result of a run: the engine's final state and any pending approvals. */
export interface WorkflowRunResult {
  /** The engine's final run state — a nested JSON payload. */
  output: unknown;
  /** Node ids left waiting on a human approval, if any. */
  pendingApprovals: string[];
  /**
   * One row per report-delivery attempt. Optional on the type (not the wire)
   * so a response from a host predating issue #170 still parses.
   */
  deliveries?: DeliveryReport[];
}

/**
 * One finished workflow run, read back from the company's journal (issue #228).
 *
 * Before this, a run's outcome existed only in the moment: a manual run's
 * `deliveries` lived in the run drawer until it was dismissed, and a scheduled
 * run's reached only the host's stdout — which on a hosted tenant is the
 * platform team, not the operator. This is the same information, durable, so it
 * survives a console reload and a scheduled run nobody watched.
 */
export interface WorkflowRunOutcome {
  /** The journal sequence position — a stable, monotonic row key. */
  seq: number;
  /** Epoch-millis the outcome was recorded. */
  atMillis: number;
  workflowId: string;
  /** True when a cron started the run rather than an operator. */
  scheduled: boolean;
  /** A run correlation id, when the entry point minted one. */
  runId?: string;
  /** The same delivery rows a manual run's response carries. */
  deliveries: DeliveryReport[];
  /** Node ids the run left waiting on a human approval. */
  pendingApprovals: string[];
  /** Set when the run failed outright instead of finishing with rows. */
  error?: string;
}

export function listWorkflows(
  client: OpenCompanyClient,
  company: string | null,
): Promise<WorkflowSummary[]> {
  return client.get<WorkflowSummary[]>(`${client.scopeFor(company)}/workflows`);
}

export function getWorkflow(
  client: OpenCompanyClient,
  company: string | null,
  wid: string,
): Promise<WorkflowGraph> {
  return client.get<WorkflowGraph>(
    `${client.scopeFor(company)}/workflows/${encodeURIComponent(wid)}`,
  );
}

export function runWorkflow(
  client: OpenCompanyClient,
  company: string | null,
  wid: string,
  input?: unknown,
): Promise<WorkflowRunResult> {
  return client.post<WorkflowRunResult>(
    `${client.scopeFor(company)}/workflows/${encodeURIComponent(wid)}/run`,
    { input: input ?? {} },
  );
}

/**
 * The company's finished workflow runs, **newest first** (issue #228).
 *
 * `workflow` narrows to one graph's runs; `limit` caps the page (the host
 * defaults to a short recent list and clamps a large ask). A host predating this
 * route answers 404 — callers should treat that as "no history yet" rather than
 * an error, since the console still works without it.
 */
export function listWorkflowRuns(
  client: OpenCompanyClient,
  company: string | null,
  options?: { workflow?: string; limit?: number },
): Promise<WorkflowRunOutcome[]> {
  const params = new URLSearchParams();
  if (options?.workflow) params.set("workflow", options.workflow);
  if (options?.limit) params.set("limit", String(options.limit));
  const query = params.toString();
  return client.get<WorkflowRunOutcome[]>(
    `${client.scopeFor(company)}/workflows/runs${query ? `?${query}` : ""}`,
  );
}

/**
 * Authors a new workflow graph (issues #69, #168): the console's form creator
 * posts the same shape `getWorkflow` returns, and the host persists it on the
 * company record — so this works on every deployment, including a hosted tenant
 * whose company source tree is a read-only mount. Rejections carry a
 * prosumer-language `ApiError` message (bad id, duplicate id or name, an edge or
 * `agent` node the graph can't support).
 */
export function createWorkflow(
  client: OpenCompanyClient,
  company: string | null,
  graph: WorkflowGraph,
): Promise<WorkflowGraph> {
  return client.post<WorkflowGraph>(`${client.scopeFor(company)}/workflows`, graph);
}

/**
 * Replaces a saved workflow graph wholesale (issue #259) — the fix for a
 * workflow being write-once, so a typo'd cron or a node pointed at the wrong
 * teammate can be corrected instead of abandoned.
 *
 * `graph.id` must equal `wid`: a workflow's id keys its saved graph, its
 * schedule and its run history, so the host rejects a rename with a `400`. A
 * rename is a create plus a delete.
 *
 * Pass `expectedVersion` — the `version` from the {@link getWorkflow} this edit
 * was based on — to make the write conditional. If the graph moved in between,
 * the host answers `409` and **nothing is written**; surface that to the
 * operator with a reload rather than retrying without the token, which is the
 * silent-overwrite the guard exists to prevent.
 *
 * Other rejections carry the same prosumer-language `ApiError` a create does:
 * `400` for a bad graph, `404` for an unknown id, `409` for a source-defined
 * workflow or a display name already taken.
 *
 * Returns the stored graph with a **fresh** `version`, so a second save needs no
 * intervening read.
 */
export function updateWorkflow(
  client: OpenCompanyClient,
  company: string | null,
  wid: string,
  graph: WorkflowGraph,
  expectedVersion?: string,
): Promise<WorkflowGraph> {
  return client.put<WorkflowGraph>(
    `${client.scopeFor(company)}/workflows/${encodeURIComponent(wid)}`,
    expectedVersion ? { ...graph, expectedVersion } : graph,
  );
}

/**
 * Removes a saved workflow (issue #259): the graph body and its entry in the
 * company's enabled list, in one write, so it leaves the picker and stops
 * firing on its schedule — and stays gone across a host restart.
 *
 * **Past runs are kept.** They record what the workflow did, which stays true
 * after it is gone; {@link listWorkflowRuns} keeps serving them.
 *
 * `expectedVersion` makes the delete conditional in exactly the sense the
 * operator means by clicking Delete on a graph they are looking at: if it
 * changed underneath them, the host answers `409` and removes nothing.
 *
 * Follows the same runtime-vs-source contract as `deleteDesk`: a workflow
 * defined by a file in the company source tree cannot be removed from the
 * console and returns `409`; an unknown id is `404`.
 */
export function deleteWorkflow(
  client: OpenCompanyClient,
  company: string | null,
  wid: string,
  expectedVersion?: string,
): Promise<void> {
  const query = expectedVersion
    ? `?expectedVersion=${encodeURIComponent(expectedVersion)}`
    : "";
  return client.del<void>(
    `${client.scopeFor(company)}/workflows/${encodeURIComponent(wid)}${query}`,
  );
}

/**
 * The node kinds the form creator's palette offers. These are the kinds that
 * are meaningful to author from a bare form — no per-node config required to do
 * something useful: `merge` fans several inputs into one stream and `transform`
 * passes items through (a config-less `set` is an identity pass-through).
 *
 * Deliberately withheld until the P4 config forms land: `tool_call`,
 * `http_request`, `switch`, `output_parser`, and `sub_workflow` all need config
 * (a slug, a URL, case labels, a schema, a `workflow_id`) to run — creating one
 * from a bare palette would silently produce a node that errors at run time — so
 * the creator doesn't offer them yet. All of these kinds still render on the
 * canvas and can be authored by hand in `workflows/<id>.toml`.
 */
/**
 * What a trigger's cron expression actually means, per the host's parser
 * (issue #262).
 *
 * Exactly one of `error` or (`description`, `next`) is present — the host
 * answers with two shapes, not one shape with half its fields null.
 * `description` is `null` for a schedule the host declines to paraphrase; the
 * fire times still say what it means.
 */
export interface CronPreview {
  /** A plain-English gloss, or `null` when the shape is too gnarly for one. */
  description?: string | null;
  /**
   * The next few fire times as epoch millis. The UTC reading and the viewer's
   * local reading are BOTH rendered from these numbers, so the two can never
   * disagree about the same instant — which is the entire point, since the
   * schedule is UTC and the author is usually not.
   */
  next?: number[];
  /** The parser's message, when the expression did not parse. */
  error?: string;
}

/**
 * Previews a cron expression (issue #262).
 *
 * **Answers 200 even for a malformed expression**, with the parser's message in
 * `error`. The console calls this while the author is still typing, so a
 * half-written expression is the normal state — and {@link OpenCompanyClient}
 * throws on any non-2xx, so a 400 per keystroke would make an ordinary parse
 * failure arrive as a thrown error. Callers still need a `catch` for genuine
 * network failure; they should render nothing in that case rather than block
 * authoring on a preview.
 *
 * `after` pins the instant the fire times are computed from; the host defaults
 * to now.
 */
export function previewCron(
  client: OpenCompanyClient,
  company: string | null,
  expr: string,
  after?: number,
): Promise<CronPreview> {
  return client.post<CronPreview>(
    `${client.scopeFor(company)}/workflows/cron/preview`,
    { expr, after },
  );
}

export const CREATABLE_NODE_KINDS: { value: string; label: string }[] = [
  { value: "trigger", label: "Trigger — starts the workflow" },
  { value: "agent", label: "Agent — a teammate performs a step" },
  { value: "condition", label: "Condition — branches on something" },
  { value: "merge", label: "Merge — combines several inputs into one" },
  { value: "transform", label: "Transform — reshapes the data" },
  { value: "output", label: "Output — reports the result back" },
];
