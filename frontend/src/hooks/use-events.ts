import { useEffect, useRef } from "react";
import { toast } from "sonner";

import type { OpenCompanyClient } from "@/api/client";
import type { DeliveryReport } from "@/api/workflows";

/**
 * One attention item off the company → operator SSE feed (issue #66). Mirrors
 * the safe projection emitted by `project_event` in `src/server/operator.rs`:
 * every field here already exists on a `CompanyEvent`, and no token, secret, or
 * raw third-party payload is ever on the wire.
 */
export type CompanyStreamEvent =
  | {
      type: "agent_reply";
      seq: number;
      atMillis: number;
      chatId: string;
      agentId: string;
      text: string;
      /**
       * The board card this reply is about (issue #246/#185) — the card the
       * turn opened, or the dispatched card it ran for. Absent on an ordinary
       * chat reply.
       */
      taskId?: string;
    }
  | { type: "task_dispatched"; seq: number; atMillis: number; taskId: string }
  | { type: "task_steered"; seq: number; atMillis: number; taskId: string; action: string }
  | {
      type: "mcp_call_failed";
      seq: number;
      atMillis: number;
      server: string;
      tool: string;
      status: string;
      message: string;
    }
  | { type: "approval_resolved"; seq: number; atMillis: number; approvalId: string; verdict: string }
  | { type: "lifecycle_changed"; seq: number; atMillis: number; from: string; to: string }
  | { type: "payment_received"; seq: number; atMillis: number; amountUsd: number; memo: string }
  // A workflow run finished (issue #228), from either entry point. The whole
  // point is the *scheduled* case: nobody is watching a cron run, so without
  // this an owner summary that failed to send would be silent until someone
  // happened to open the history panel.
  | {
      type: "workflow_run_finished";
      seq: number;
      atMillis: number;
      workflowId: string;
      scheduled: boolean;
      deliveries: DeliveryReport[];
      pendingApprovals: string[];
      /** Present only when the run failed outright. */
      error?: string;
    }
  // The transient live turn-progress frames (`src/turn_stream.rs`): a tool call
  // just started (status `running`) or finished (status `ok`/`error`). These are
  // ephemeral — never journaled — and drive the in-flight tool timeline the
  // console renders *while a turn runs*. `label`/`detail` are already scrubbed at
  // the source (same rules as the folded `TurnStep`s), so no raw args/output.
  // `chatId` is the chat/desk thread the turn answers — the same id the durable
  // `agent_reply` carries. The console keys the live tool timeline on it so
  // concurrent turns on different threads never cross-attribute their frames.
  | {
      type: "tool_call";
      seq: number;
      agentId?: string;
      chatId?: string;
      toolCallId?: string;
      label?: string;
      status?: string;
    }
  | {
      type: "tool_result";
      seq: number;
      agentId?: string;
      chatId?: string;
      toolCallId?: string;
      label?: string;
      detail?: string;
      status?: string;
      elapsedMs?: number;
    }
  // A coalesced "Thinking" run between tool calls — streamed so the live
  // timeline shows the same rows the final folded one does (else the count
  // jumps up when the reply lands).
  | { type: "thinking"; seq: number; agentId?: string; chatId?: string };

/** An `AgentReply` the hook hands back for injection into a chat transcript. */
export interface AgentReplyEvent {
  chatId: string;
  agentId: string;
  text: string;
  /** The board card this reply opened (issue #246), when it opened one. */
  taskId?: string;
}

interface Options {
  /**
   * The number of approvals currently awaiting the operator, from the existing
   * status poll. A rising edge fires the "needs a sign-off" push — approvals have
   * no `CompanyEvent`, so this is the one attention signal that rides the poll
   * rather than the SSE stream.
   */
  pendingApprovals: number;
  /**
   * Called for each `AgentReply` so the shell can inject it into the active
   * chat's transcript. The shell dedupes against its own optimistic echo.
   */
  onAgentReply?: (event: AgentReplyEvent) => void;
  /**
   * Called for each task-lifecycle event (`task_dispatched`, `task_steered`) so
   * a surface showing in-flight runs — the company-chat steer strip (issue #111)
   * — can refetch live off the existing SSE stream instead of only on a poll.
   */
  onTaskEvent?: (event: CompanyStreamEvent) => void;
  /**
   * Called for each live turn-progress frame (`tool_call`, `tool_result`) so the
   * chat can render the tool timeline as the turn runs, then reconcile against
   * the folded steps on the final reply.
   */
  onTurnEvent?: (event: CompanyStreamEvent) => void;
  /**
   * Called for each `workflow_run_finished` event (issue #228) so the Workflows
   * view can refresh its run history live. Matters most for a *scheduled* run:
   * it fires with the tab already open and nothing else would tell the view a
   * new outcome exists.
   */
  onWorkflowRunEvent?: (event: CompanyStreamEvent) => void;
}

/**
 * Opens an `EventSource` on `{scope}/events` for the active company and turns
 * incoming attention events into `sonner` toasts (and, for agent replies, a
 * transcript injection via {@link Options.onAgentReply}). This is the active
 * push half of the attention surface; the passive 5s status/approvals poll in
 * {@link useCompany} stays as the fallback — if the host doesn't expose
 * `/events` (404) or the connection drops, this hook degrades silently and the
 * poll keeps the console current.
 */
export function useEvents(
  client: OpenCompanyClient,
  company: string | null,
  {
    pendingApprovals,
    onAgentReply,
    onTaskEvent,
    onTurnEvent,
    onWorkflowRunEvent,
  }: Options,
): void {
  // Keep the latest callbacks without re-opening the stream when they change.
  const onAgentReplyRef = useRef(onAgentReply);
  useEffect(() => {
    onAgentReplyRef.current = onAgentReply;
  }, [onAgentReply]);
  const onTaskEventRef = useRef(onTaskEvent);
  useEffect(() => {
    onTaskEventRef.current = onTaskEvent;
  }, [onTaskEvent]);
  const onTurnEventRef = useRef(onTurnEvent);
  useEffect(() => {
    onTurnEventRef.current = onTurnEvent;
  }, [onTurnEvent]);
  const onWorkflowRunEventRef = useRef(onWorkflowRunEvent);
  useEffect(() => {
    onWorkflowRunEventRef.current = onWorkflowRunEvent;
  }, [onWorkflowRunEvent]);

  // The rising-edge detector for pending approvals. Seeded with the current
  // value so we only toast on an *increase* observed while mounted, never on the
  // first read or when the count falls after a resolution.
  const prevPending = useRef(pendingApprovals);
  useEffect(() => {
    if (pendingApprovals > prevPending.current) {
      toast.warning("Your company needs a sign-off", {
        description:
          pendingApprovals === 1
            ? "An action is waiting for your approval."
            : `${pendingApprovals} actions are waiting for your approval.`,
      });
    }
    prevPending.current = pendingApprovals;
  }, [pendingApprovals]);

  // The SSE subscription. Re-opens when the company (or client) changes.
  useEffect(() => {
    // EventSource can only speak same-origin cookies; the URL is built from the
    // client's base + scope so it lands on the right company under either
    // deployment shape.
    const url = `${client.baseUrl}${client.scopeFor(company)}/events`;
    let source: EventSource;
    try {
      source = new EventSource(url, { withCredentials: true });
    } catch (err) {
      // A malformed URL or an environment without EventSource: nothing to do,
      // the poll remains the source of truth.
      console.debug("[events] EventSource unavailable, falling back to poll", err);
      return;
    }
    console.debug("[events] connecting", { url });

    source.onopen = () => {
      console.debug("[events] connected", { url });
    };

    source.onmessage = (msg) => {
      let event: CompanyStreamEvent;
      try {
        event = JSON.parse(msg.data) as CompanyStreamEvent;
      } catch (err) {
        console.debug("[events] dropping unparseable event", err);
        return;
      }
      handleEvent(
        event,
        onAgentReplyRef.current,
        onTaskEventRef.current,
        onTurnEventRef.current,
        onWorkflowRunEventRef.current,
      );
    };

    source.onerror = () => {
      // On a 404 / wrong content-type the browser closes the stream and does not
      // reconnect (readyState === CLOSED); on a transient drop it reconnects on
      // its own. Either way we log and lean on the poll — no manual retry loop.
      const closed = source.readyState === EventSource.CLOSED;
      console.debug("[events] stream error", {
        url,
        reconnecting: !closed,
      });
      if (closed) source.close();
    };

    return () => {
      console.debug("[events] disconnecting", { url });
      source.close();
    };
  }, [client, company]);
}

/** Routes one parsed event to its toast / transcript side effect. */
function handleEvent(
  event: CompanyStreamEvent,
  onAgentReply?: (e: AgentReplyEvent) => void,
  onTaskEvent?: (e: CompanyStreamEvent) => void,
  onTurnEvent?: (e: CompanyStreamEvent) => void,
  onWorkflowRunEvent?: (e: CompanyStreamEvent) => void,
): void {
  switch (event.type) {
    // Live turn frames drive the in-flight tool timeline — no toast, they render
    // inline in the chat.
    case "tool_call":
    case "tool_result":
    case "thinking":
      onTurnEvent?.(event);
      break;
    case "mcp_call_failed":
      toast.error(`MCP ${event.server} failed`, {
        description: event.message || `${event.tool} · ${event.status}`,
      });
      break;
    case "task_dispatched":
      toast("A task is on the move", {
        description: "Your company picked up a task.",
      });
      onTaskEvent?.(event);
      break;
    case "task_steered":
      toast("A task was steered", {
        description: `Your company ${steeredVerb(event.action)} a task.`,
      });
      onTaskEvent?.(event);
      break;
    case "agent_reply":
      onAgentReply?.({
        chatId: event.chatId,
        agentId: event.agentId,
        text: event.text,
        // Issue #246: a reply injected from the stream — one this console did
        // not POST for, e.g. an inbound Telegram turn — carries its "card
        // opened" chip too, rather than only the locally-awaited copy.
        taskId: event.taskId,
      });
      break;
    case "approval_resolved":
      toast(event.verdict === "approve" ? "Approval granted" : "Approval denied", {
        description: "An approval was just resolved.",
      });
      break;
    case "lifecycle_changed":
      toast(`Company is now ${event.to}`, {
        description: `Changed from ${event.from}.`,
      });
      break;
    case "payment_received":
      toast.success("Payment received", {
        description: `$${event.amountUsd.toFixed(2)} — ${event.memo}`,
      });
      break;
    // Issue #228. A run that went fine is not an attention signal — toasting
    // every scheduled run would train the operator to ignore the ones that
    // matter — so only a failure or an undelivered report speaks up. The view
    // still refreshes either way, so a clean run shows up in the history.
    case "workflow_run_finished": {
      onWorkflowRunEvent?.(event);
      if (event.error) {
        toast.error("A workflow run failed", {
          description: `${event.workflowId} — ${event.error}`,
        });
        break;
      }
      // `pending` is not a failure: it is a report parked for approval, and
      // toasting it red would send the operator hunting for a bug that isn't
      // there. Excluded from the count that speaks up.
      //
      // Compared through a widened `string` because `pending` joins
      // `DeliveryStatus` in issue #227 — a literal would be a no-overlap type
      // error against today's union, and the host can already send a status
      // this console's type doesn't name yet.
      const pendingStatus: string = "pending";
      const undelivered = event.deliveries.filter(
        (d) => d.status !== "sent" && d.status !== pendingStatus,
      ).length;
      if (undelivered > 0) {
        toast.warning(
          `${undelivered} report${undelivered === 1 ? "" : "s"} didn't go out`,
          {
            description: `${event.workflowId}${
              event.scheduled ? " (scheduled run)" : ""
            } — open Workflows for the reason.`,
          },
        );
      }
      break;
    }
    default:
      // An unknown/forward event kind: ignore rather than surface noise.
      break;
  }
}

/** Past-tense phrasing for a `task_steered` action, for the toast copy. */
function steeredVerb(action: string): string {
  switch (action) {
    case "pause":
      return "paused";
    case "cancel":
      return "cancelled";
    case "redirect":
      return "redirected";
    default:
      return "steered";
  }
}
