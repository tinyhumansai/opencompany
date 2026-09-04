/**
 * The run-observability read: what a company's agents actually did.
 *
 * Hand-written documents and hand-written result types, in the style every other
 * module in `src/api/` uses. See `graphql.ts` for why there is no client
 * library.
 *
 * The join this exists for did not have an answer until a workflow `agent` node
 * started minting an attempt row: a node's turn has neither a card nor a
 * conversation, so `RunStore` — keyed on exactly those — could not name it.
 */

import type { OpenCompanyClient } from "@/api/client";
import { runQuery } from "@/api/graphql";

/** Token and cost totals for one attempt. */
export interface ObservatoryUsage {
  inputTokens: number;
  outputTokens: number;
  cachedInputTokens: number;
  costUsd: number;
}

/**
 * The unredacted companion of one step.
 *
 * Every field is `| null` rather than optional, deliberately: `null` is a case
 * the renderer must *handle* — "this host keeps no deep trace", or "this step
 * had none" — and an optional field is one a renderer can forget.
 */
export interface ObservatoryDeep {
  reasoning: string | null;
  arguments: string | null;
  output: string | null;
  displayDetail: string | null;
  iteration: number | null;
  clipped: boolean;
}

/** One step of an attempt's trace. */
export interface ObservatoryStep {
  seq: number;
  atMillis: number;
  /** `tool_call` | `thinking` | `note`. */
  kind: string;
  /** `ok` | `error` | `running` | `awaiting_approval`. */
  status: string;
  label: string;
  /** Arguments, through the host redactor. Always safe to render. */
  detail: string | null;
  /** A summary or shape of the result — never a remote body. */
  result: string | null;
  failure: string | null;
  truncated: boolean;
  elapsedMs: number | null;
  /** `null` when the host keeps no deep trace, and when the step produced none. */
  deep: ObservatoryDeep | null;
}

/** One attempt at work — a card dispatch, a chat turn, or a workflow node. */
export interface ObservatoryRun {
  id: string;
  agentId: string;
  attempt: number;
  status: string;
  /** `active` | `parked` | `terminal` — read this rather than inferring one. */
  phase: string;
  taskId: string | null;
  chatId: string | null;
  workflowRunId: string | null;
  nodeId: string | null;
  createdAtMillis: number;
  startedAtMillis: number | null;
  finishedAtMillis: number | null;
  error: string | null;
  usage: ObservatoryUsage;
  /**
   * The settled count, or `null` while the attempt is live.
   *
   * Null is load-bearing: `stepCount` is written by the settle, so a live
   * attempt has no honest total and the host refuses to invent one. Count
   * `steps` instead — see `stepTotal`.
   */
  stepCount: number | null;
  steps: ObservatoryStep[];
}

/** How many steps an attempt has, live or settled. */
export function stepTotal(run: ObservatoryRun): number {
  return run.stepCount ?? run.steps.length;
}

/** Whether an attempt is still going. */
export function isLive(run: ObservatoryRun): boolean {
  return run.phase === "active";
}

/**
 * The run shape for a query. `deep` adds the unredacted half — reasoning, raw
 * tool arguments and raw output — which **carries secrets by construction**, so
 * it is selected only for the single run a reader actually opened, never for a
 * list. Who may read it at all is decided server-side, beside the approval rule
 * (`approval_visibility.rs`); the console asks and the host answers.
 */
function runFields(deep: boolean): string {
  return `
  id
  agentId
  attempt
  status
  phase
  taskId
  chatId
  workflowRunId
  nodeId
  createdAtMillis
  startedAtMillis
  finishedAtMillis
  error
  usage { inputTokens outputTokens cachedInputTokens costUsd }
  stepCount
  steps {
    seq
    atMillis
    kind
    status
    label
    detail
    result
    failure
    truncated
    elapsedMs
    ${deep ? "deep { reasoning arguments output displayDetail iteration clipped }" : ""}
  }
`;
}

const RUN_FIELDS = runFields(false);
const RUN_FIELDS_WITH_DEEP = runFields(true);

const RUNS_QUERY = `
  query ObservatoryRuns($company: ID!, $workflowRunId: ID, $taskId: ID, $limit: Int!) {
    company(id: $company) {
      agentRuns(workflowRunId: $workflowRunId, taskId: $taskId, limit: $limit) {
        ${RUN_FIELDS}
      }
    }
  }
`;

const RUN_QUERY = `
  query ObservatoryRun($company: ID!, $id: ID!) {
    company(id: $company) { agentRun(id: $id) { ${RUN_FIELDS_WITH_DEEP} } }
  }
`;

interface RunsResult {
  company: { agentRuns: ObservatoryRun[] } | null;
}

interface RunResult {
  company: { agentRun: ObservatoryRun | null } | null;
}

/** Every attempt a workflow run's nodes spawned, newest first. */
export async function fetchRunsForWorkflowRun(
  client: OpenCompanyClient,
  company: string,
  workflowRunId: string,
  limit = 50,
): Promise<ObservatoryRun[]> {
  const data = await runQuery<RunsResult>(
    client,
    RUNS_QUERY,
    { company, workflowRunId, taskId: null, limit },
    company,
  );
  return data.company?.agentRuns ?? [];
}

/** Recent attempts across the company, whatever spawned them. */
export async function fetchRecentRuns(
  client: OpenCompanyClient,
  company: string,
  limit = 50,
): Promise<ObservatoryRun[]> {
  const data = await runQuery<RunsResult>(
    client,
    RUNS_QUERY,
    { company, workflowRunId: null, taskId: null, limit },
    company,
  );
  return data.company?.agentRuns ?? [];
}

/**
 * One attempt by id — **with its unredacted half**, or `null` when the company
 * does not have it.
 *
 * This is the on-demand deep read: the list queries select no deep bodies, so
 * the console fetches this only when a reader opens an attempt's card. The deep
 * fields resolve to `null` per step for a host that keeps no deep trace, and
 * for a reader who may not see one.
 */
export async function fetchRun(
  client: OpenCompanyClient,
  company: string,
  id: string,
): Promise<ObservatoryRun | null> {
  const data = await runQuery<RunResult>(client, RUN_QUERY, { company, id }, company);
  return data.company?.agentRun ?? null;
}
