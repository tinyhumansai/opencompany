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
      /**
       * The message this reply belongs under (issue #364) — its thread inside
       * the channel. Absent for a reply in the channel itself, and on a host
       * that predates persisted threads.
       */
      parentId?: string;
    }
  | { type: "task_dispatched"; seq: number; atMillis: number; taskId: string }
  | { type: "task_steered"; seq: number; atMillis: number; taskId: string; action: string }
  // A board card was written (issue #464) — the frame the board had no way to
  // learn about anything from. Emitted by the host's task store, so it fires
  // for a card opened from chat intake, from a delegation, from the publish
  // drain and from the REST route alike, rather than for whichever paths
  // somebody remembered to instrument.
  //
  // Deliberately thin, like `approval_parked`: an id, what happened, and where
  // the card sits. There is **no title and no note** on purpose — the card's
  // text lives on `GET …/tasks`, and a console reacts to this frame by
  // re-reading that, so the board's content has exactly one source.
  | {
      type: "task_card_changed";
      seq: number;
      atMillis: number;
      taskId: string;
      /** `opened` | `updated` | `removed`, widened so an unknown word from a
       *  newer host is not a type error. */
      change: string;
      /** Absent on a removed card, which is in no column. */
      column?: string;
    }
  // A dispatched card's run finished (issue #185). The host has projected this
  // since #185; the console named no type for it, so it fell through to
  // `default:` and was dropped — the same "the view does not subscribe" half of
  // #464, one event over. A settle moves the card between columns, so the board
  // wants it.
  | {
      type: "desk_task_completed";
      seq: number;
      atMillis: number;
      taskId: string;
      desk: string;
      output: string;
      column: string;
    }
  | {
      type: "mcp_call_failed";
      seq: number;
      atMillis: number;
      server: string;
      tool: string;
      status: string;
      message: string;
    }
  // A request just parked (issue #379), so a console watching the conversation
  // it came from can raise the card live instead of waiting for its next
  // approvals poll.
  //
  // Deliberately thin, and mirrored from the host: an id, the effect's dotted
  // kind, and the chat thread it was raised in. There is **no payload and no
  // asker** here on purpose — the effect's arguments are redacted in exactly one
  // place (`GET …/approvals`), so the console reacts to this frame by refreshing
  // that feed and renders the card from the redacted summary.
  //
  // `chatId` is absent when no conversation produced the approval (a workflow
  // delivery, a scheduler tick, anything parked before #379). Such an approval
  // matches no channel and stays on the Approvals page, exactly as before.
  | {
      type: "approval_parked";
      seq: number;
      atMillis: number;
      approvalId: string;
      kind: string;
      chatId?: string;
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
      /** The run's correlation id (issue #371). Absent on a pre-#371 row. */
      runId?: string;
      /**
       * Present only when an operator stopped the run (issue #383).
       *
       * A stopped run carries NO `error`, so without reading this a console
       * would render a cancel as a clean success — and tell the operator who
       * just pressed Cancel that the run finished fine.
       */
      cancelled?: boolean;
    }
  // Issue #371: the live per-node progress trail. A run announces itself, then
  // reports each non-trigger node as it finishes, so the canvas can show which
  // nodes are done while the run is still going — the whole point of the issue.
  //
  // There is deliberately no *node started* frame: the engine's observer has no
  // such hook, so "currently executing" is derived from the graph topology the
  // console already holds.
  | {
      type: "workflow_run_started";
      seq: number;
      atMillis: number;
      workflowId: string;
      runId: string;
      scheduled: boolean;
    }
  | {
      type: "workflow_node_finished";
      seq: number;
      atMillis: number;
      workflowId: string;
      runId: string;
      nodeId: string;
      /** Widened past the host's two words so an unknown status can't be a type error. */
      status: string;
      elapsedMs: number;
    }
  // Issue #384: a saved workflow was authored, edited or removed — by the
  // orchestrator's `create_workflow` tool, by a second console session, by a
  // machine credential, or by this console itself. The host has journalled and
  // projected all three since #112/#259; nothing here named them, so they
  // reached `default:` and were dropped, and the picker drifted from what the
  // host holds until a reload.
  //
  // Deliberately thin, like `approval_parked` and `task_card_changed`: an id
  // and the display name, with **no graph body** — the host omits it on purpose
  // (see `project_event`). A console reacts to this frame by re-reading
  // `GET …/workflows`, so the picker's content keeps exactly one source.
  | {
      type: "workflow_created" | "workflow_updated" | "workflow_deleted";
      seq: number;
      atMillis: number;
      workflowId: string;
      name: string;
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
  /**
   * The **host-side** id of this message (issue #483) — the stream envelope's
   * `seq`. `chat/history` projects its own `id` from the same `StoredEvent`
   * sequence, so a live line stamped with this carries the identity a later
   * rehydration will mint for it, and the two can be recognised as one message
   * instead of both being rendered.
   *
   * Namespaced into a console id by the injector, same as {@link parentId}.
   */
  seq: number;
  /** The board card this reply opened (issue #246), when it opened one. */
  taskId?: string;
  /**
   * The **host-side** id of the message this reply belongs under (issue #364).
   * Namespaced into a console id by the injector, which is what knows about the
   * `h` prefix.
   */
  parentId?: string;
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
   * Called for each task-lifecycle event (`task_dispatched`, `task_steered`,
   * `task_card_changed`, `desk_task_completed`) so a surface showing board work
   * — the company-chat steer strip (issue #111) and the board itself (issue
   * #464) — can refetch live off the existing SSE stream instead of only on a
   * poll.
   *
   * Subscribers here take a **counter**, not the payload: they re-read the
   * board, and the board's content has one source (`GET …/tasks`). That is also
   * why they are immune to the frame-loss trap that made
   * {@link Options.onWorkflowRunEvent}'s consumer fold a *window* of events —
   * two ticks collapsing inside one React batch still means "re-read", whereas
   * two payloads collapsing means one is lost.
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
  /**
   * Called for each workflow **authoring** frame — `workflow_created`,
   * `workflow_updated`, `workflow_deleted` (issue #384) — so the Workflows view
   * can re-read its picker live.
   *
   * Separate from {@link Options.onWorkflowRunEvent} because they answer
   * different questions: that one is what a run *did*, this one is which
   * workflows *exist*. A view can want either without the other, and folding
   * them would make the run canvas re-derive itself on an unrelated create.
   *
   * Like {@link Options.onTaskEvent} this subscriber takes a **counter**, not
   * the payload: it re-reads the list, so two frames collapsing inside one
   * React batch still means "re-read". That is also why a delete needs nothing
   * extra on the wire — the workflow that vanished is the one the refreshed
   * list no longer has.
   */
  onWorkflowChanged?: (event: CompanyStreamEvent) => void;
  /**
   * Called for each approval-lifecycle frame (`approval_parked`,
   * `approval_resolved`) so the shell can refresh the approvals feed live
   * (issue #379).
   *
   * Both directions matter and for opposite reasons. A **park** is what puts a
   * card into the conversation as it happens — the whole point of the issue.
   * A **resolution** is what settles a card decided from the *other* surface:
   * approve on the Approvals page and the inline copy has to stop offering
   * buttons for a decision that is already made.
   */
  onApprovalEvent?: (event: CompanyStreamEvent) => void;
}

/**
 * The subscriber half of {@link Options}, as {@link handleEvent} takes it.
 *
 * A named bag rather than a positional list, because five of the six share the
 * type `(e: CompanyStreamEvent) => void` — so a call site that got the ORDER
 * wrong would type-check perfectly and route every frame to the wrong surface.
 * Derived from `Options` rather than restated so the two cannot drift, and so
 * each callback keeps the documentation written against it above.
 */
type Subscribers = Omit<Options, "pendingApprovals">;

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
    onWorkflowChanged,
    onApprovalEvent,
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
  const onWorkflowChangedRef = useRef(onWorkflowChanged);
  useEffect(() => {
    onWorkflowChangedRef.current = onWorkflowChanged;
  }, [onWorkflowChanged]);
  const onApprovalEventRef = useRef(onApprovalEvent);
  useEffect(() => {
    onApprovalEventRef.current = onApprovalEvent;
  }, [onApprovalEvent]);

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
      handleEvent(event, {
        onAgentReply: onAgentReplyRef.current,
        onTaskEvent: onTaskEventRef.current,
        onTurnEvent: onTurnEventRef.current,
        onWorkflowRunEvent: onWorkflowRunEventRef.current,
        onWorkflowChanged: onWorkflowChangedRef.current,
        onApprovalEvent: onApprovalEventRef.current,
      });
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
function handleEvent(event: CompanyStreamEvent, subscribers: Subscribers): void {
  const {
    onAgentReply,
    onTaskEvent,
    onTurnEvent,
    onWorkflowRunEvent,
    onWorkflowChanged,
    onApprovalEvent,
  } = subscribers;
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
    // Issue #464. Both route to the board's subscriber and **neither toasts**,
    // which is the deliberate half.
    //
    // A card opened from chat is already announced where the operator is
    // looking — the reply says "Card opened" — so a toast for it would be the
    // second notification for one action that #379 argues against. And a board
    // write is not rare: every column move, every settle, every note the run
    // appends is one. Toasting those would train the operator to dismiss the
    // toasts that do matter. The card appearing on the board IS the
    // notification.
    case "task_card_changed":
    case "desk_task_completed":
      onTaskEvent?.(event);
      break;
    case "agent_reply":
      onAgentReply?.({
        chatId: event.chatId,
        agentId: event.agentId,
        text: event.text,
        // Issue #483: the host's own id for this message. Carried so the
        // injected line and its later rehydrated twin share an identity.
        seq: event.seq,
        // Issue #246: a reply injected from the stream — one this console did
        // not POST for, e.g. an inbound Telegram turn — carries its "card
        // opened" chip too, rather than only the locally-awaited copy.
        taskId: event.taskId,
        // Issue #364: and lands in the same thread a reload would put it in,
        // rather than arriving in the channel and jumping on the next refresh.
        parentId: event.parentId,
      });
      break;
    // Issue #379. No toast: the rising-edge "needs a sign-off" toast off the
    // poll already covers the attention half, and the card appearing in the
    // channel IS the notification. Two of them for one request would be the
    // noise this issue exists to remove.
    case "approval_parked":
      onApprovalEvent?.(event);
      break;
    case "approval_resolved":
      // Refresh first, then say so. The refresh is what settles an inline card
      // whose decision was made on the Approvals page (or in another tab).
      onApprovalEvent?.(event);
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
      // Issue #383. A stop somebody asked for is not an alarm, so this is an
      // acknowledgement rather than an error toast — but it must not fall
      // through to the "everything went fine" path either, which would tell the
      // operator who pressed Cancel that the run completed. Checked BEFORE the
      // delivery scan because a cancelled run has no deliveries to scan.
      if (event.cancelled) {
        toast.info("A workflow run was stopped", {
          description: `${event.workflowId} — stopped by an operator. Steps that finished are in the run history.`,
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
      // `deliveries` is host-controlled and may be absent on an event shape this
      // console's types don't name yet (see note above) — default to empty so a
      // missing field can never throw and blank the subscriber.
      const undelivered = (event.deliveries ?? []).filter(
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
    // Issue #371: progress frames route to the same subscriber the finished
    // event does, and deliberately raise NO toast. Progress is not an attention
    // signal — a six-node run would fire eight toasts and train the operator to
    // dismiss the one that matters. The canvas is where this belongs.
    //
    // These arms exist because this file has already been bitten by events
    // falling through to `default:` and vanishing; a new event type without an
    // arm here is silently dropped, with nothing to debug.
    case "workflow_run_started":
    case "workflow_node_finished":
      onWorkflowRunEvent?.(event);
      break;
    // Issue #384: the authoring half. Same subscriber-refreshes-its-own-surface
    // shape as the run arms above, and the third time this file has grown arms
    // for frames the host was already sending — #464 for the board, #371 for
    // the canvas, these for the picker.
    //
    // Deliberately NO toast. A workflow appearing, being renamed or going away
    // is not an attention signal: the orchestrator authors them as ordinary
    // work, and a toast per create would be noise of exactly the kind #379
    // argues against. The list refreshing IS the feedback.
    case "workflow_created":
    case "workflow_updated":
    case "workflow_deleted":
      onWorkflowChanged?.(event);
      break;
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
