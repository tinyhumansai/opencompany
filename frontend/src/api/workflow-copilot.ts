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

import type { OpenCompanyClient } from "./client";
import { PROPOSAL_FENCE } from "./workflow-proposal";
import type {
  WorkflowGraph,
  WorkflowRunOutcome,
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
export function composeCopilotMessage(context: CopilotContext, question: string): string {
  const { graph, runs, runsKnown } = context;
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
    graph.description ? `Description: ${graph.description}` : `Description: (none)`,
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
    if (node.schedule) bits.push(`schedule (5-field cron, UTC): ${node.schedule}`);
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
      lines.push(`- ${edge.from} → ${edge.to}${edge.label ? ` [${edge.label}]` : ""}`);
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
      `- {"op": "addNode", "node": {"id": "…", "kind": "…", "name": "…", …}} — a new step. Use the same fields the steps above show.`,
      `- {"op": "updateNode", "id": "…", "set": {"field": value}} — change fields on an existing step. Only send the fields that change.`,
      `- {"op": "removeNode", "id": "…"} — delete a step (its connections go with it).`,
      `- {"op": "addEdge", "from": "…", "to": "…", "label": "optional"} — connect two steps.`,
      `- {"op": "removeEdge", "from": "…", "to": "…"} — disconnect two steps.`,
      `Rules: refer to steps by the ids listed above; never rename an id (that is a remove plus an add); propose the smallest change that answers the question; and say in your prose what the change does and why. If you are not confident enough to propose, say so and describe the change instead — a wrong proposal costs the operator more than no proposal.`,
    );
  }

  // Joined with the marker verbatim, so `questionOf` splits on exactly the
  // string this function used.
  return `${lines.join("\n")}${QUESTION_MARKER}${question}`;
}

/** One run as a single grounding line — the three terminal readings issue #383
 * separated, kept distinct here too, plus where a failure landed. */
function describeRun(run: WorkflowRunOutcome): string {
  const when = new Date(run.atMillis).toISOString();
  const how = run.scheduled ? "scheduled" : "manual";
  const nodes = run.nodes ?? [];
  const trail = nodes.length
    ? ` steps: ${nodes.map((n) => `${n.nodeId}=${n.status}(${n.elapsedMs}ms)`).join(", ")};`
    : "";
  const undelivered = run.deliveries.filter((d) => d.status !== "sent" && d.status !== "pending");
  const delivery = run.deliveries.length
    ? ` deliveries: ${run.deliveries.map((d) => `${d.node}→${d.kind}=${d.status}`).join(", ")};`
    : "";
  const outcome = run.running
    ? "still running"
    : run.error
      ? `FAILED: ${run.error}`
      : run.cancelled
        ? "stopped by an operator before it finished"
        : undelivered.length
          ? `finished, but ${undelivered.length} report(s) were not delivered`
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
  const reply = await client.chat(message, company, copilotThreadId(workflowId));
  return reply.responses.map((r) => r.text);
}

/**
 * Replays a workflow's copilot transcript from the company journal.
 *
 * Returns an empty transcript on ANY failure: a host predating
 * `…/chat/history` answers 404, and a copilot that refuses to open because it
 * could not replay old messages would be worse than one that starts blank.
 */
export async function loadCopilotHistory(
  client: OpenCompanyClient,
  company: string | null,
  workflowId: string,
): Promise<CopilotMessage[]> {
  try {
    const rows = await client.getChatHistory(copilotThreadId(workflowId), company);
    return rows.map((row) => ({
      id: row.id,
      role: row.mine ? "operator" : "company",
      // Strip the inlined grounding back off the operator's side, so a
      // rehydrated transcript reads like the conversation the operator had.
      text: row.mine ? questionOf(row.text) : row.text,
      atMillis: row.atMillis,
    }));
  } catch (e) {
    console.debug("[workflow-copilot] no transcript to replay", e);
    return [];
  }
}
