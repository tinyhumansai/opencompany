import { useEffect, useRef } from "react";
import { toast } from "sonner";

import type { OpenCompanyClient } from "@/api/client";

/**
 * One attention item off the company → operator SSE feed (issue #66). Mirrors
 * the safe projection emitted by `project_event` in `src/server/operator.rs`:
 * every field here already exists on a `CompanyEvent`, and no token, secret, or
 * raw third-party payload is ever on the wire.
 */
export type CompanyStreamEvent =
  | { type: "agent_reply"; seq: number; atMillis: number; chatId: string; agentId: string; text: string }
  | { type: "task_dispatched"; seq: number; atMillis: number; taskId: string }
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
  | { type: "payment_received"; seq: number; atMillis: number; amountUsd: number; memo: string };

/** An `AgentReply` the hook hands back for injection into a chat transcript. */
export interface AgentReplyEvent {
  chatId: string;
  agentId: string;
  text: string;
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
  { pendingApprovals, onAgentReply }: Options,
): void {
  // Keep the latest callback without re-opening the stream when it changes.
  const onAgentReplyRef = useRef(onAgentReply);
  useEffect(() => {
    onAgentReplyRef.current = onAgentReply;
  }, [onAgentReply]);

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
      handleEvent(event, onAgentReplyRef.current);
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
function handleEvent(event: CompanyStreamEvent, onAgentReply?: (e: AgentReplyEvent) => void): void {
  switch (event.type) {
    case "mcp_call_failed":
      toast.error(`MCP ${event.server} failed`, {
        description: event.message || `${event.tool} · ${event.status}`,
      });
      break;
    case "task_dispatched":
      toast("A task is on the move", {
        description: "Your company picked up a task.",
      });
      break;
    case "agent_reply":
      onAgentReply?.({ chatId: event.chatId, agentId: event.agentId, text: event.text });
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
    default:
      // An unknown/forward event kind: ignore rather than surface noise.
      break;
  }
}
