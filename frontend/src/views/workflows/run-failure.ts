// The durable record of a run this console dispatched and the host rejected
// (issue #1007).
//
// Pure and React-free for the same reason `run-error.ts` is: before this, a
// failed run's entire trace on screen was `toast.error(e.message)` — four
// seconds of prose over a console that had otherwise returned to its resting
// state. A value is what makes "the failure is still there" something a test
// can assert, and what lets the panel say more than a toast had room for.
//
// `run-error.ts` decides WHICH failure this was; this decides what is left on
// screen once it has. The two are deliberately separate: the triage has three
// answers and only one of them lands here.

import { ApiError } from "@/api/types";

/** Everything the failure panel renders, captured at the moment of the catch. */
export interface RunFailure {
  /**
   * The prose the operator reads. Never empty: a thrown non-Error, or an Error
   * whose message is blank, still has to say something — a panel with no
   * sentence in it is the vanishing toast again, only quieter.
   */
  message: string;
  /**
   * The host's structured code, and ONLY when it came from the host's own
   * `{error, code}` envelope (issue #380's `fromHost`). A code synthesised from
   * a status line names the status, not the fault, and printing it beside the
   * message would dress a proxy timeout up as a host verdict.
   */
  code?: string;
  /**
   * The HTTP status, when the request completed at all. `0` is the client's
   * marker for "it never did" and is kept rather than normalised away — it is
   * the difference between a host that answered and a connection that died.
   */
  status?: number;
  /** Whether the host itself refused, rather than something between it and the
   * browser giving up. Decides which of the two closing sentences is true. */
  fromHost: boolean;
  /** When the operator pressed Run, so the panel can say how long it ran for. */
  startedAtMillis: number;
  /** When the failure was caught. */
  atMillis: number;
  /** What the operator asked this run for (issue #154); `""` when nothing. */
  request: string;
  /** A test run (issue #542) — this failure spent nothing and sent nothing. */
  dryRun: boolean;
}

/** What the caller knows that the thrown value does not. */
export interface RunFailureContext {
  startedAtMillis: number;
  atMillis: number;
  request: string;
  dryRun: boolean;
}

/**
 * Builds the panel's record from whatever `runWorkflow` threw.
 *
 * Reads the STRUCTURED shape where there is one and degrades to the message
 * otherwise — the same rule the triage next door follows, for the same reason:
 * a `fetch` that rejects with a `TypeError`, a mocked client that throws a
 * string, and a host 500 all have to leave the operator with a panel.
 */
export function runFailureFrom(e: unknown, ctx: RunFailureContext): RunFailure {
  const base = {
    startedAtMillis: ctx.startedAtMillis,
    atMillis: ctx.atMillis,
    request: ctx.request,
    dryRun: ctx.dryRun,
  };
  if (e instanceof ApiError) {
    return {
      ...base,
      message: e.message || `The host answered ${e.status} with no explanation.`,
      code: e.fromHost ? e.code : undefined,
      status: e.status,
      fromHost: e.fromHost,
    };
  }
  return {
    ...base,
    message:
      (e instanceof Error ? e.message : String(e ?? "")) ||
      "The run failed, and nothing said why.",
    fromHost: false,
  };
}
