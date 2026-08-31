// Issue #303: the per-workflow copilot — a chat scoped to ONE saved workflow.
//
// ## No new host route, on purpose
//
// This talks through the company's existing chat surface — `POST {scope}/chat`,
// via {@link OpenCompanyClient.chat} — addressed to a thread id derived from the
// workflow. Three properties of the host make that a real seam rather than a
// smuggling route, and all three are load-bearing:
//
// 1. **It routes to the orchestrator.** The brain resolves an addressed message
//    to that desk's lead, else a roster agent of that name, else the
//    orchestrator. `workflow-copilot:<id>` is neither a desk nor an agent, so it
//    falls to the orchestrator — which is the agent that holds whole-company
//    context. `client.chat`'s own doc states this contract: "Omitted / unknown
//    ids fall to the orchestrator, so callers can always pass the active thread
//    id safely."
// 2. **It is isolated in the journal.** Replies are journaled against the
//    addressed thread, and the host's desk filter matches the thread id
//    exactly — the General desk's catch-all arm applies only when General is the
//    desk being *read*. So a copilot exchange never appears in the team's chat,
//    and one workflow's copilot never sees another's. That is the issue's
//    "no cross-workflow leakage" criterion, enforced server-side rather than by
//    this file remembering to filter.
//
//    **It isolates TRANSCRIPTS.** One workflow's copilot never sees another's,
//    and none of it appears in the team's chat.
//
//    Until issue #416 that was the *whole* of the scoping: the thread id picked
//    the responder and the journal and narrowed nothing else, so the teammate
//    answering was the company orchestrator with its full context and tools,
//    and "answer only about this workflow" was an instruction in the prompt —
//    advice, not a boundary. It cost nothing in privilege (the operator can
//    reach the same orchestrator from the Chat tab), but it meant an answer
//    about a workflow could be drawn from anywhere in the company.
//
//    The host now reads the thread itself. `workflow-copilot:<id>` runs a
//    **confined turn**: an ephemeral agent with no tools, no company memory and
//    no delegation, whose whole world is the message composed below. See
//    `src/company/copilot.rs` (the convention) and `src/harness/confine.rs`
//    (the boundary), plus `docs/spec/runtime/api.md`.
// 3. **It rehydrates for free.** Because the exchange is journaled under that
//    thread, `GET {scope}/chat/history?desk=<thread>` replays it after a reload,
//    with no new storage and no new route.
//
// A manifest desk id is letters, digits and underscores, so the `:` in the
// prefix means a copilot thread can never collide with a real desk — and it does
// not appear in `GET {scope}/desks`, so it never shows up as a chat thread in
// the console's own chat view either.
//
// ## Why the graph is inlined in every message
//
// The orchestrator's `query_company` tool lists saved workflows as `(id, name)`
// pairs and nothing more; **no agent tool can read a workflow's nodes and edges,
// and none can read run history at all** (past runs are REST-only). So an
// unaided "what does this workflow do?" would be answered from the name. The
// console holds the full graph and the workflow's own run history already, so it
// sends them as context — which is also what keeps the answer scoped to exactly
// one workflow.
//
// It is re-sent on every turn rather than once per session. Whether the
// orchestrator sees the thread's earlier messages is a property of the brain,
// not a contract this console can rely on; re-sending costs tokens and is
// correct either way, where sending once is cheaper and wrong if that assumption
// ever fails.

import { NODE_CONFIG_FIELDS } from "@/lib/workflow-node-config";
import type { OpenCompanyClient } from "./client";
import { isDetachedChat } from "./types";
import { PROPOSAL_FENCE } from "./workflow-proposal";
import {
  WORKFLOW_NODE_KINDS,
  type UnwiredWorkflowTool,
  type WorkflowGraph,
  type WorkflowRunOutcome,
} from "./workflows";

/**
 * Separates the inlined context from what the operator actually typed.
 *
 * It is a display concern, not a prompt one: the whole composed message is what
 * gets journaled, so a rehydrated transcript would otherwise show the operator
 * "saying" several hundred lines of JSON. {@link questionOf} splits it back off.
 */
const QUESTION_MARKER = "\n\n### The operator's question\n";

/** How many recent runs are summarised into the context. */
const CONTEXT_RUNS = 10;

/**
 * The chat thread a workflow's copilot converses on.
 *
 * Deterministic, so a reload finds the same transcript, and prefixed so it can
 * never be a real desk id.
 */
export function copilotThreadId(workflowId: string): string {
  return `workflow-copilot:${workflowId}`;
}

/** One turn in the copilot transcript, as the panel renders it. */
export interface CopilotMessage {
  /** Stable within a session; the journal's message id once rehydrated. */
  id: string;
  role: "operator" | "company";
  text: string;
  atMillis: number;
}

/**
 * Everything the copilot is allowed to see about the workflow it is opened on.
 *
 * Deliberately a closed set. The copilot is grounded in exactly this and the
 * operator's question — never another workflow's graph, and never company data
 * the console did not already have on screen.
 */
export interface CopilotContext {
  graph: WorkflowGraph;
  /** The workflow's OWN runs, newest first — the server-scoped history read. */
  runs: WorkflowRunOutcome[];
  /**
   * Whether the host served run history at all.
   *
   * Distinguishes "this workflow has never run" from "this host has no run
   * history to give" — both arrive here as an empty `runs`, and telling the
   * model the first when the second is true would have it reason confidently
   * from an absence that means nothing.
   */
  runsKnown: boolean;
  /**
   * The company roster (agent id → role) an `agent` step may name — issue #783.
   *
   * Without it a proposed `agent` node named a teammate the model invented, and
   * the host refused the write. The console already holds this from `GET …/team`,
   * so grounding the copilot on the same list is what stops the guess. Optional:
   * a host predating the roster read leaves it absent, and the message says the
   * roster couldn't be listed rather than claiming there is nobody.
   */
  roster?: RosterEntry[];
  /**
   * The **effective** `tool_call` slugs a proposed tool step may run — issue
   * #783, narrowed by issue #874. Granted by the company AND wired on this
   * deployment (`GET …/workflows/tool-slugs`), so the model proposes a real tool
   * instead of guessing `github_integration` — and, since #874, not one that is
   * granted but has no provider here and would fail at the first run.
   */
  toolSlugs?: string[];
  /**
   * Whether the host served the tool-slug list at all.
   *
   * Same honesty split as {@link runsKnown}: an empty `toolSlugs` on a host that
   * serves the route means "no tools are granted", and the model should not
   * propose a `tool_call`; an absent list (old host, 404) means "cannot say",
   * and telling the model "no tools" would be a lie that suppresses a legitimate
   * proposal. `false`/absent keeps the message from making the stronger claim.
   */
  toolSlugsKnown?: boolean;
  /**
   * Tools this company is granted but that are not wired here — issue #874.
   *
   * Listed as an advisory the model must not author from, mirroring the
   * create-time copilot's `list_effective_tools`. Naming them is what lets the
   * copilot answer "this needs web search, which is not configured on this
   * deployment" instead of either proposing a doomed node or claiming the
   * company has no such tool at all.
   */
  unwiredTools?: UnwiredWorkflowTool[];
}

/** One roster teammate a proposed `agent` step may name — id and role. */
export interface RosterEntry {
  id: string;
  role: string;
}

/**
 * Composes the message actually sent to the host: the workflow's graph and
 * recent runs as grounding, then the operator's question.
 *
 * Exported for the panel's "what can it see?" disclosure — an operator is
 * entitled to know exactly what was sent on their behalf, and building that
 * from the same function that sends it means the disclosure cannot drift from
 * the truth.
 */
export function composeCopilotMessage(
  context: CopilotContext,
  question: string,
): string {
  const {
    graph,
    runs,
    runsKnown,
    roster,
    toolSlugs,
    toolSlugsKnown,
    unwiredTools,
  } = context;
  // `editable: false` means the graph is defined by a file in the company
  // source tree. Named for what it IS rather than for the flag's polarity: this
  // was `editable`, holding the negation of the field it was named after, and
  // #415 adds two more branches keyed off it (whether to describe the proposal
  // protocol at all, and what to tell the model it may do).
  const sourceDefined = graph.editable === false;

  const lines: string[] = [
    `You are the workflow copilot for this company, answering about ONE saved workflow.`,
    // The host confines this turn (issue #416): it runs on an agent with no
    // tools and no company memory, so this is a description of the turn's real
    // boundary rather than an instruction it could step outside of. Kept in the
    // message as well as in the host-side persona so the disclosure the panel
    // shows the operator is built from the same text that was actually sent.
    `Everything you know about it is below. You have no tools and no access to the rest of the company, so answer from this material only.`,
    `If the question needs a different workflow or the wider company, say which part you cannot answer here and that it has to be asked in the company chat. Do not guess, and do not claim to have looked anything up.`,
    ``,
    `## Workflow`,
    `Name: ${graph.name}`,
    `Id: ${graph.id}`,
    graph.description
      ? `Description: ${graph.description}`
      : `Description: (none)`,
    sourceDefined
      ? `Editable from the console: NO — this workflow is defined by a file in the company source tree (workflows/${graph.id}.toml). You can explain it, but changes have to be made in the company repository, not here.`
      : `Editable from the console: yes, through the workflow editor.`,
    ``,
    `## Graph`,
    `Nodes (${graph.nodes.length}):`,
  ];

  for (const node of graph.nodes) {
    const bits = [`- ${node.id} [${node.kind}] "${node.name}"`];
    if (node.summary) bits.push(`summary: ${node.summary}`);
    if (node.agent) bits.push(`agent: ${node.agent}`);
    if (node.schedule)
      bits.push(`schedule (5-field cron, UTC): ${node.schedule}`);
    if (node.requiresApproval) bits.push(`requires approval before proceeding`);
    if (node.onError) bits.push(`on error: ${node.onError}`);
    if (node.retry) bits.push(`retry: ${JSON.stringify(node.retry)}`);
    if (node.destination) {
      bits.push(
        `destination: ${node.destination.kind}${
          node.destination.target ? ` → ${node.destination.target}` : ""
        }`,
      );
    }
    if (node.config !== undefined && node.config !== null) {
      bits.push(`config: ${JSON.stringify(node.config)}`);
    }
    lines.push(bits.join(" · "));
  }

  lines.push(``, `Edges (${graph.edges.length}):`);
  if (graph.edges.length === 0) {
    lines.push(`- (none — the nodes are not joined)`);
  } else {
    for (const edge of graph.edges) {
      lines.push(
        `- ${edge.from} → ${edge.to}${edge.label ? ` [${edge.label}]` : ""}`,
      );
    }
  }

  lines.push(``, `## Recent runs`);
  const recent = runs.slice(0, CONTEXT_RUNS);
  if (!runsKnown) {
    // Not the same as "it has never run", and the difference matters: a model
    // told "no runs" will happily conclude the workflow has never been used.
    lines.push(
      `This host does not serve run history, so nothing is known about whether this workflow has run. Do not infer that it has never run.`,
    );
  } else if (recent.length === 0) {
    // Said exactly this way because it is what the console knows. The history
    // read is scoped to this workflow server-side, so "none recorded" here is a
    // stronger and more honest statement than the cards can make.
    lines.push(`No runs are recorded for this workflow.`);
  } else {
    for (const run of recent) {
      lines.push(`- ${describeRun(run)}`);
    }
  }

  // Issue #783: ground the proposal path in the company's REAL roster and tools,
  // and in the per-kind config schema. Without these three the model guessed —
  // it named teammates and tools that don't exist, and put kind-specific keys
  // (`slug`, `url`, `repo`) as top-level node fields the host's allowlist then
  // rejected. All three are inlined so the confined turn (no tools, no memory)
  // can see them.
  lines.push(``, `## Company grounding`);

  lines.push(
    ``,
    `### Roster — the teammate ids an \`agent\` step may name`,
    `An \`agent\` node names its teammate with the top-level \`agent\` field (a roster id below), NOT inside \`config\`.`,
  );
  if (roster === undefined) {
    lines.push(
      `(The roster could not be listed here. Do not invent teammate ids.)`,
    );
  } else if (roster.length === 0) {
    lines.push(
      `(This company has no roster teammates, so do not propose an \`agent\` step.)`,
    );
  } else {
    for (const member of roster) {
      lines.push(`- ${member.id}${member.role ? ` — ${member.role}` : ""}`);
    }
  }

  lines.push(
    ``,
    `### Tools — the slugs a \`tool_call\` step may run`,
    `A \`tool_call\` node names its tool with \`config.slug\`, set to one of these EXACT slugs. Do not invent a slug (there is no \`github_integration\`, etc.).`,
  );
  // The single predicate both the list and its advisory key off. Hoisted rather
  // than spelled out twice: the advisory is only coherent directly under a list
  // that was actually read, so the two conditions must be exact negations of one
  // another — and two copies of `toolSlugsKnown && toolSlugs !== undefined` is
  // precisely how they drift apart.
  const slugsListed = toolSlugsKnown && toolSlugs !== undefined;
  if (!slugsListed) {
    lines.push(
      `(The tools that can run here could not be listed. Do not invent a tool slug.)`,
    );
  } else if (toolSlugs.length === 0) {
    lines.push(
      `(No tools can run in this company's workflows, so do not propose a \`tool_call\` step.)`,
    );
  } else {
    for (const slug of toolSlugs) lines.push(`- ${slug}`);
  }

  // Issue #874: named, but firmly out of bounds. The list above is already
  // narrowed to what can run here, so without this the model cannot tell a tool
  // the company was never granted from one that is merely unconfigured — and it
  // should answer "search is not wired on this deployment" rather than either
  // proposing a node that dies at the first run or denying the tool exists.
  //
  // Gated on `slugsListed`, so the pairing cannot come apart: the advisory is a
  // NARROWING of the list above, and there is nothing to narrow when that list
  // could not be read. Emitting it anyway would say "these are off-limits"
  // directly under "the tools that can run here could not be listed", which is
  // self-contradictory on its face — and, since the caller holds the two in
  // separate state, the tools named would be whichever company was on screen
  // last. The condition closes that by construction rather than by the caller
  // remembering to clear one when it clears the other.
  if (slugsListed && unwiredTools !== undefined && unwiredTools.length > 0) {
    lines.push(
      ``,
      `### Granted but NOT wired on this deployment — do NOT author these`,
      `These cannot run here. If the change the operator wants needs one, say so and name the reason instead of proposing it.`,
    );
    for (const tool of unwiredTools) {
      lines.push(`- ${tool.slug} — ${tool.detail}`);
    }
  }

  lines.push(
    ``,
    `## What you can and cannot do`,
    `You can explain this workflow, diagnose why its runs failed, and describe in words what should change.`,
    `You CANNOT reach anything else in the company: no tools, no board, no teammates, no other workflow, no files. The host enforces this, so a call would be refused rather than answered.`,
    // Issue #415. The proposal is DATA IN THE REPLY, not a capability: it is
    // the operator's console that writes, through the same versioned
    // `updateWorkflow` the editor uses, only after they have read the diff. So
    // this instruction hands the model no reach it did not have — which is
    // exactly why the confinement (#416) had to land first.
    sourceDefined
      ? `You CANNOT edit the workflow, and you must not propose an edit: this one is defined by a file in the company source tree, so the host refuses every write to it. Describe the change for someone to make in the company repository.`
      : `You CANNOT apply a change yourself. What you CAN do is PROPOSE one: the operator reads it as a diff and applies it, or throws it away.`,
  );

  if (!sourceDefined) {
    lines.push(
      ``,
      `## Proposing a change`,
      `When the operator asks for a change (and only then), end your reply with ONE fenced block of exactly this form, after your prose:`,
      "```" + PROPOSAL_FENCE,
      `{"summary": "one line saying what this does", "ops": [ … ]}`,
      "```",
      `Each op is one of:`,
      `- {"op": "addNode", "node": {"id": "…", "kind": "…", "name": "…", …}} — a new step.`,
      `- {"op": "updateNode", "id": "…", "set": {"field": value}} — change fields on an existing step. Only send the fields that change.`,
      `- {"op": "removeNode", "id": "…"} — delete a step (its connections go with it).`,
      `- {"op": "addEdge", "from": "…", "to": "…", "label": "optional"} — connect two steps.`,
      `- {"op": "removeEdge", "from": "…", "to": "…"} — disconnect two steps.`,
      ``,
      // Issue #783 — the shape rules the model kept getting wrong. Every proposal
      // used to be rejected because kind-specific keys landed as top-level fields
      // and referenced nodes/tools/teammates that didn't exist.
      `### A step's kind and its config`,
      `\`kind\` must be exactly one of: ${WORKFLOW_NODE_KINDS.join(", ")}. Pick the right one:`,
      `- \`agent\` — a teammate does the work. Name them in the top-level \`agent\` field (a roster id above).`,
      `- \`tool_call\` — run one wired tool. Name it in \`config.slug\` (a tool slug above).`,
      `- \`output\` — report the result back. No tool and no teammate.`,
      `- \`condition\` / \`switch\` — branch. \`http_request\` — call a URL. \`sub_workflow\` — run another saved workflow.`,
      ``,
      `Kind-specific keys go INSIDE a \`config\` object on the node — NEVER as top-level fields. The top-level node fields are only: id, kind, name, summary, agent, schedule, config, onError, retry, requiresApproval, destination. Everything else (slug, url, args, method, field, expression, workflow_id, schema, …) lives in \`config\`. For example a tool call is:`,
      `{"op": "addNode", "node": {"id": "search", "kind": "tool_call", "name": "Search the web", "config": {"slug": "web_search", "args": {"query": "=item.topic"}}}}`,
      ``,
      `The \`config\` keys each kind reads (a key marked (required) must be present):`,
      ...configCatalogLines(),
      ``,
      `### Rules`,
      `- To reference an EXISTING step (updateNode, removeNode, and the from/to of addEdge/removeEdge), use only an id listed under ## Graph above — never one that is not there.`,
      `- An addNode mints a NEW id that is deliberately not yet in the graph: make it short, lower-case and unique. That is the only place a not-yet-present id is allowed.`,
      `- Never rename an id (that is a remove plus an add).`,
      `- Only name a teammate from the roster above, and only a tool slug from the tools above.`,
      `- Propose the smallest change that answers the question, and say in your prose what it does and why.`,
      `- If you are not confident enough to propose, say so and describe the change instead — a wrong proposal costs the operator more than no proposal.`,
    );
  }

  // Joined with the marker verbatim, so `questionOf` splits on exactly the
  // string this function used.
  return `${lines.join("\n")}${QUESTION_MARKER}${question}`;
}

/**
 * The per-kind `config` schema, rendered from {@link NODE_CONFIG_FIELDS} so the
 * catalog the model is taught cannot drift from the fields the console actually
 * authors and validates (issue #783). One line per kind that has config keys,
 * each key marked `(required)` where the host requires it.
 */
function configCatalogLines(): string[] {
  const lines: string[] = [];
  for (const [kind, specs] of Object.entries(NODE_CONFIG_FIELDS)) {
    const keys = specs
      .map((spec) => `config.${spec.key}${spec.required ? " (required)" : ""}`)
      .join(", ");
    lines.push(`- ${kind}: ${keys}`);
  }
  return lines;
}

/** One run as a single grounding line — the three terminal readings issue #383
 * separated, kept distinct here too, plus where a failure landed, plus the
 * `degraded` reading issue #1865 added for a run that settled clean but left
 * an errored node behind it. */
function describeRun(run: WorkflowRunOutcome): string {
  const when = new Date(run.atMillis).toISOString();
  const how = run.scheduled ? "scheduled" : "manual";
  const nodes = run.nodes ?? [];
  const trail = nodes.length
    ? ` steps: ${nodes.map((n) => `${n.nodeId}=${n.status}(${n.elapsedMs}ms)`).join(", ")};`
    : "";
  const undelivered = run.deliveries.filter(
    (d) => d.status !== "sent" && d.status !== "pending",
  );
  const delivery = run.deliveries.length
    ? ` deliveries: ${run.deliveries.map((d) => `${d.node}→${d.kind}=${d.status}`).join(", ")};`
    : "";
  // Issue #881: the blocked reading is named before the delivery one and before
  // the bare "finished". A blocked run has no error, is not cancelled and
  // routed nothing, so without this arm it grounded the copilot with the word
  // "finished" about a run that produced no deliverable — and the copilot would
  // then reason from that as fact.
  const blocked = run.blockedNodes ?? [];
  const parked = run.approvals?.length ?? 0;
  const erroredNodes = nodes.filter((n) => n.status === "error");
  // Issue #1865 (PR #1883 review): the LAST reading, exactly where both
  // `WorkflowRunVerdict::of` (host) and `verdictOf` (console) rank it — after
  // every arm above, all of which describe something more actionable. A node
  // under `on_error: continue|route` errored and the graph kept going past
  // it, or an agent node's turn truncated at the iteration cap; either way
  // the run has no top-level `error`, is not cancelled and blocked nobody, so
  // without this arm it fell through to the bare "finished" below while its
  // own step trail named an errored node — the exact contradiction (`finished;
  // steps: agent=error(...)`) issue #881 closed for `blocked` above, just
  // reachable through the newer verdict. Prefers the host's own word; the
  // `erroredNodes` fallback covers a host predating #1865 that nevertheless
  // sent the node trail, the same "prefer the host's word, fall back to the
  // signal it reads" shape {@link verdictOf} uses in `run-health.ts`.
  //
  // PR #1883 review (codex, comment 3886484125): gated on an empty
  // `pendingApprovals` — both `WorkflowRunVerdict::of` and `verdictOf` check
  // `awaiting_count`/`awaitingCount` BEFORE this legacy fallback, because a
  // node parked on a native `requiresApproval` gate leaves no `blockedNodes`
  // row (only a gated-call block does), so a run can carry an errored
  // continue/route node AND a live approval card at once. Without this guard
  // that run read as `DEGRADED: … not a clean finish` while it was actually
  // sitting open, waiting on an operator — a stronger, wronger claim than the
  // "finished" this arm exists to replace, on the run most likely to still be
  // actionable.
  //
  // PR #1883 review (codex, comment 3892522597): `pendingApprovals` is only
  // HALF of what the host's `awaiting_count` reads — the other half is a
  // `pending` delivery, which `fully_stranded`'s own doc calls "a *second*
  // thing waiting on a person, on its own queue... untouched by the gate
  // join". A cold-recipient email output node (`ParkedForApproval`) parks the
  // REPORT for approval without ever touching `pendingApprovals` at all — that
  // gate lives entirely on the delivery row. So a run can carry an errored
  // continue/route node, an EMPTY `pendingApprovals`, and a `pending`
  // delivery all at once — exactly the case the guard above does not catch.
  // Without this second guard that run also read as `DEGRADED`, the same
  // stronger-and-wronger claim over a run genuinely waiting on an operator to
  // approve a report, not on a node fix.
  const awaitingDelivery = run.deliveries.some((d) => d.status === "pending");
  const degraded =
    run.pendingApprovals.length === 0 &&
    !awaitingDelivery &&
    (run.verdict === "degraded" || (run.verdict === undefined && erroredNodes.length > 0));
  const outcome = run.running
    ? "still running"
    : run.error
      ? `FAILED: ${run.error}`
      : run.cancelled
        ? "stopped by an operator before it finished"
        : blocked.length
          ? `BLOCKED at ${blocked.map((b) => b.nodeId).join(", ")} — produced no deliverable and the steps after did not run; parked ${parked} approval(s), which does NOT continue this run`
          : undelivered.length
            ? `finished, but ${undelivered.length} report(s) were not delivered`
            : degraded
              ? `DEGRADED: ${erroredNodes.map((n) => n.nodeId).join(", ") || "a step"} errored but the graph continued past it — not a clean finish`
              : "finished";
  const approvals = run.pendingApprovals.length
    ? ` awaiting approval: ${run.pendingApprovals.join(", ")};`
    : "";
  return `${when} (${how}) — ${outcome};${trail}${delivery}${approvals}`;
}

/**
 * The operator-visible half of a composed message.
 *
 * Uses the **FIRST** occurrence of the marker, because that is the one
 * {@link composeCopilotMessage} inserted: everything before it is context this
 * file wrote, and the question is everything after. Scanning from the end
 * instead — as this did originally — finds a marker the operator typed inside
 * their own question and returns only the tail, silently dropping the start of
 * what they asked. The comment there claimed the opposite of what the code
 * did.
 *
 * A message with no marker is returned whole — that is either a plain chat
 * message journaled on this thread by something else, or a pre-#303 row.
 */
export function questionOf(text: string): string {
  const at = text.indexOf(QUESTION_MARKER);
  return at === -1 ? text : text.slice(at + QUESTION_MARKER.length);
}

/**
 * Asks the copilot a question and returns the company's replies.
 *
 * Throws whatever {@link OpenCompanyClient} throws — a paused company is a 409,
 * an unreachable host is a network error. Callers surface those; there is no
 * degraded mode worth inventing, because a copilot that silently answers nothing
 * is the failure this whole surface is meant to avoid.
 */
export async function askCopilot(
  client: OpenCompanyClient,
  company: string | null,
  workflowId: string,
  context: CopilotContext,
  question: string,
): Promise<string[]> {
  const message = composeCopilotMessage(context, question);
  const answer = await client.chat(
    message,
    company,
    copilotThreadId(workflowId),
  );
  // The copilot does not ask to detach (issue #983): its whole contract is to
  // return the answer, and it has no stream to watch or transcript row to
  // re-arm. Narrowed rather than cast — a `202` here would mean the host
  // detached something nobody asked to detach, and an empty answer is a
  // truthful "no reply" instead of a crash on a missing field.
  if (isDetachedChat(answer)) return [];
  return answer.responses.map((r) => r.text);
}

/**
 * Replays a workflow's copilot transcript from the company journal.
 *
 * A failed replay is reported separately from an empty transcript so the panel
 * can leave the copilot usable while making the degraded history visible.
 */
export async function loadCopilotHistory(
  client: OpenCompanyClient,
  company: string | null,
  workflowId: string,
): Promise<
  { ok: true; messages: CopilotMessage[] } | { ok: false; error: Error }
> {
  try {
    const rows = await client.getChatHistory(
      copilotThreadId(workflowId),
      company,
      { limit: 200 },
    );
    return {
      ok: true,
      messages: rows.map((row) => ({
        id: row.id,
        role: row.mine ? "operator" : "company",
        // Strip the inlined grounding back off the operator's side, so a
        // rehydrated transcript reads like the conversation the operator had.
        text: row.mine ? questionOf(row.text) : row.text,
        atMillis: row.atMillis,
      })),
    };
  } catch (e) {
    return {
      ok: false,
      error:
        e instanceof Error
          ? e
          : new Error("The copilot transcript could not be loaded."),
    };
  }
}
