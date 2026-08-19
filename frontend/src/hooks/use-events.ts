import { useEffect, useRef } from "react";
import { toast } from "sonner";

import type { OpenCompanyClient } from "@/api/client";
import type {
  DeliveryReport,
  WorkflowBlockedNode,
  WorkflowRunApprovalRow,
} from "@/api/workflows";

/**
 * One attention item off the company → operator SSE feed (issue #66). Mirrors
 * the safe projection emitted by `project_event` in `src/server/operator.rs`:
 * every field here already exists on a `CompanyEvent`, and no token, secret, or
 * raw third-party payload is ever on the wire.
 */
export type CompanyStreamEvent =
  // A structural control frame, never a journal event. The receiver fell
  // behind, so durable views must re-read their canonical endpoints.
  | { type: "stream_gap"; missed: number }
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
  | {
      type: "task_steered";
      seq: number;
      atMillis: number;
      taskId: string;
      action: string;
    }
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
  // One task attempt moved (issue #1015). Before this the task detail screen
  // could only learn an attempt's status by polling every four seconds — and not
  // at all while the tab was hidden — which is the surface #581 removed
  // elsewhere when it replaced the whole-company refetch with Snapshot +
  // Refresh.
  //
  // Deliberately thin, like `task_card_changed` beside it: ids, the status, and
  // where it came from. There is **no error text** on purpose — a failure reason
  // is tenant-scoped, so a console reacts to this frame by re-reading the run
  // row, which is the one place that answers *why*.
  | {
      type: "run_status_changed";
      seq: number;
      atMillis: number;
      runId: string;
      /** Absent for a chat turn, which is an attempt at work with no card. */
      taskId?: string;
      /** 1-based attempt ordinal at that card. */
      attempt: number;
      /** The status moved to, widened so a word from a newer host is not a type
       *  error. */
      status: string;
      /** The status moved from. Absent on the mint, which has no prior state —
       *  a presence check, matching `turn_started`'s `parentId`. */
      from?: string;
    }
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
  // A workspace node was written (issue #327) — created, overwritten, moved or
  // deleted. `task_card_changed`'s counterpart for the note tree, and missing
  // for the same reason: the Workspace tab could only learn about a write by
  // refetching on refresh or refocus, which is a long time to be wrong now that
  // agents create notes and published deliverables land in the tree.
  //
  // Deliberately thin, like its board sibling: an id and what happened. There
  // is **no node name and no body** on purpose — a note's text lives on
  // `GET …/workspace`, and the view reacts to this frame by re-reading that, so
  // the tree's content has exactly one source.
  | {
      type: "workspace_changed";
      seq: number;
      atMillis: number;
      nodeId: string;
      /** `opened` | `updated` | `removed`, widened so an unknown word from a
       *  newer host is not a type error. */
      change: string;
    }
  // A dispatched card's run finished (issue #185/#377). The host has projected
  // this since #185; the console named no type for it until #464, so it fell
  // through to `default:` and was dropped — the same "the view does not
  // subscribe" half of #464, one event over. A settle moves the card between
  // columns, so the board wants it.
  //
  // Issue #377 made it a *chat* frame as well as a board one, and reshaped it
  // in both directions:
  //
  // - `chatId` in: the conversation the card was raised from, captured on the
  //   card at raise time. Absent when nothing raised it from a chat — a
  //   board-created card, a scheduler's. Such a settle belongs to no channel
  //   and gets no marker, exactly as a `chatId`-less `approval_parked` stays on
  //   the Approvals page.
  // - `output` out: the run's prose already reaches the same channel as the
  //   orchestrator's relay bubble, so the host stopped projecting it rather
  //   than leave a second copy on the wire for a future reader to render.
  //   Nothing in this console read it.
  | {
      type: "desk_task_completed";
      seq: number;
      atMillis: number;
      taskId: string;
      /** The *responder* that ran it — an agent id, never a channel id. */
      desk: string;
      column: string;
      /** The channel the card was raised in; absent for a board-created card. */
      chatId?: string;
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
  | {
      type: "approval_resolved";
      seq: number;
      atMillis: number;
      approvalId: string;
      verdict: string;
      /**
       * The **host** resolved this, not a person (#971) — an approval that sat
       * past its deadline and was declined on its own.
       *
       * A bit derived from the event's actor, never the actor itself: this feed
       * is deny-by-default and carries no `by` and no user id, which is a
       * property the host asserts. It answers the only question the console has
       * — "did somebody decide this?" — and answering it matters because
       * without it an expiry toasted "Approval denied", telling an operator
       * they had declined a request they never saw.
       *
       * Absent on a person's own decision and against a host that predates the
       * field. Both mean the same thing to a reader: treat it as a decision
       * somebody made, exactly as before.
       */
      automatic?: boolean;
    }
  | {
      type: "lifecycle_changed";
      seq: number;
      atMillis: number;
      from: string;
      to: string;
    }
  | {
      type: "payment_received";
      seq: number;
      atMillis: number;
      amountUsd: number;
      memo: string;
    }
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
      /**
       * System notices raised about the run (issue #638), e.g. that gated tool
       * calls past the per-batch cap were discarded. Absent on the vast
       * majority of runs. Not a failure — see `WorkflowRunOutcome.notices`.
       */
      notices?: string[];
      /**
       * The nodes this run blocked on a person (issue #881). Absent when
       * nothing blocked.
       *
       * A blocked run carries NO `error` and is not `cancelled`, so without
       * reading this a console watching a run settle would paint it green —
       * and then the history it reloads a moment later would say it blocked.
       */
      blockedNodes?: WorkflowBlockedNode[];
      /**
       * The approvals this run parked (issue #880) — a receipt of what it
       * opened, including the parks that failed.
       */
      approvals?: WorkflowRunApprovalRow[];
    }
  // Issue #371/#382: the live per-node progress trail. A run announces itself,
  // then brackets each non-trigger node with a *started* frame as it begins and
  // a *finished* frame as it settles, so the canvas can show which node is
  // executing right now and which are done while the run is still going — the
  // whole point of the issue.
  //
  // Issue #382 added the *started* frame: the engine's observer gained an
  // `on_step_start` hook, so "currently executing" is now REPORTED by the host
  // rather than derived from the graph topology the console holds.
  | {
      type: "workflow_run_started";
      seq: number;
      atMillis: number;
      workflowId: string;
      runId: string;
      scheduled: boolean;
    }
  // Issue #382: a non-trigger node began executing. Structural ids only — the
  // node has not run, so there is no status or duration, and never any input.
  | {
      type: "workflow_node_started";
      seq: number;
      atMillis: number;
      workflowId: string;
      runId: string;
      nodeId: string;
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
      type:
        | "workflow_created"
        | "workflow_updated"
        | "workflow_deleted"
        | "workflow_enabled_changed";
      seq: number;
      atMillis: number;
      workflowId: string;
      name: string;
      /**
       * `workflow_enabled_changed` only (issue #276): the state it moved to,
       * and whether a person moved it or the host's disarm rule did.
       *
       * Declared because the host sends them, not because this console reads
       * them — the subscriber below takes a counter and re-reads the list, so
       * the armed state it renders comes from `GET …/workflows` rather than
       * from a frame it might have missed.
       */
      enabled?: boolean;
      reason?: "operator" | "disarmed";
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
   * Re-reads durable state after a structural stream gap, a healthy stream
   * connection, or a stream that failed to open. The latter keeps the hosted
   * console current while the manager proxy fix in opencompany-microservice#23
   * is rolling out.
   */
  onResync?: () => Promise<void> | void;
  /** Reports a failed canonical recovery once, without turning ordinary gaps
   * or reconnects into toast noise. */
  onRecoveryError?: (error: unknown) => void;
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
   * Called for each `run_status_changed` frame (issue #1015) so a screen showing
   * one card's attempts can re-read them the moment one moves, rather than up to
   * a poll interval later.
   *
   * Separate from {@link Options.onTaskEvent} because they answer different
   * questions: that one is which cards moved on the board, this one is what one
   * card's attempt is doing. The task detail screen wants the second without
   * re-reading the whole board on every transition of every run.
   *
   * Like {@link Options.onTaskEvent} this subscriber takes a **counter**, not
   * the payload: it re-reads the detail, so two frames collapsing inside one
   * React batch still means "re-read". It is therefore immune to frame loss,
   * and the consumer keeps its poll as the fallback for a frame that never
   * arrives at all.
   */
  onRunEvent?: (event: CompanyStreamEvent) => void;
  /**
   * Called for each `desk_task_completed` frame (issue #377) so the shell can
   * post a card-linked system marker into the channel the card was raised in.
   *
   * Beside {@link Options.onTaskEvent}, not instead of it: one frame, two
   * genuinely different reactions. The board still wants its refetch tick — a
   * settle moves a card between columns — and the channel wants a line saying
   * the card settled and where. Folding them would force one subscriber to be
   * both a counter and a payload.
   *
   * Takes the **payload**, for the same reason {@link Options.onWorkspaceEvent}
   * does: the reaction depends on *which* conversation and *which* column, and
   * a counter can say neither. The frame-loss trap that made the workflow
   * canvas fold an event window does not apply — a dropped frame costs one
   * marker that the next hydration restores from history, never wrong state.
   */
  onDispatchTerminal?: (event: CompanyStreamEvent) => void;
  /**
   * Called for each `workspace_changed` frame (issue #327) so the Workspace
   * view can re-read the tree — and the open note — live.
   *
   * Unlike {@link Options.onTaskEvent}, this one takes the **payload**, not
   * just a counter. The view's reaction depends on *which* node moved: the tree
   * is always refetched, but the open note is only refetched when the frame
   * names it, and a `removed` frame naming the open note has to close the pane
   * rather than refetch a note that is gone. A counter cannot say any of that.
   *
   * The frame-loss trap that forced the workflow canvas to fold an event
   * *window* does not apply, because the tree refetch is unconditional: a
   * dropped frame costs at most one stale render of the open note, never wrong
   * tree state.
   */
  onWorkspaceEvent?: (event: CompanyStreamEvent) => void;
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
   * Called for each frame that changes what the workflow picker should show —
   * `workflow_created`, `workflow_updated`, `workflow_deleted` (issue #384) and
   * `workflow_enabled_changed` (issue #276) — so the Workflows view can re-read
   * its picker live.
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
export type Subscribers = Omit<Options, "pendingApprovals">;

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
    onRunEvent,
    onDispatchTerminal,
    onWorkspaceEvent,
    onTurnEvent,
    onWorkflowRunEvent,
    onWorkflowChanged,
    onApprovalEvent,
    onResync,
    onRecoveryError,
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
  const onRunEventRef = useRef(onRunEvent);
  useEffect(() => {
    onRunEventRef.current = onRunEvent;
  }, [onRunEvent]);
  const onDispatchTerminalRef = useRef(onDispatchTerminal);
  useEffect(() => {
    onDispatchTerminalRef.current = onDispatchTerminal;
  }, [onDispatchTerminal]);
  const onWorkspaceEventRef = useRef(onWorkspaceEvent);
  useEffect(() => {
    onWorkspaceEventRef.current = onWorkspaceEvent;
  }, [onWorkspaceEvent]);
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
  const onResyncRef = useRef(onResync);
  useEffect(() => {
    onResyncRef.current = onResync;
  }, [onResync]);
  const onRecoveryErrorRef = useRef(onRecoveryError);
  useEffect(() => {
    onRecoveryErrorRef.current = onRecoveryError;
  }, [onRecoveryError]);

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

  // The event subscription. Re-opens when the company (or client) changes.
  useEffect(() => {
    // Which wire carries this is the client's business (browser `EventSource`
    // same-origin, the desktop's own core otherwise). What arrives is the same
    // stream of frames either way, so everything below is unchanged.
    const url = `${client.baseUrl}${client.scopeFor(company)}/events`;
    let unsubscribe: () => void;
    let opened = false;
    let recoveryFailed = false;
    let recoveryInFlight = false;
    let reconcileTimer: ReturnType<typeof setInterval> | undefined;
    const recover = async () => {
      if (recoveryInFlight) return;
      recoveryInFlight = true;
      try {
        await onResyncRef.current?.();
        recoveryFailed = false;
      } catch (err) {
        // A failed canonical read is actionable, but repeated reconnects must
        // not flood the operator with the same error every thirty seconds.
        if (!recoveryFailed) onRecoveryErrorRef.current?.(err);
        recoveryFailed = true;
      } finally {
        recoveryInFlight = false;
      }
    };
    const startReconciliation = () => {
      if (reconcileTimer) return;
      void recover();
      reconcileTimer = setInterval(() => void recover(), 30_000);
    };
    // The hosted manager currently buffers the whole upstream response (#23),
    // so an EventSource can remain CONNECTING forever without firing onOpen or
    // onError. Treat that as loss of the incremental channel, not as silence.
    const openDeadline = setTimeout(() => {
      if (!opened) startReconciliation();
    }, 10_000);
    try {
      unsubscribe = client.subscribeToEvents(company, {
        onOpen: () => {
          opened = true;
          clearTimeout(openDeadline);
          if (reconcileTimer) clearInterval(reconcileTimer);
          reconcileTimer = undefined;
          console.debug("[events] connected", { url });
          void recover();
        },
        onMessage: (data) => {
          let event: CompanyStreamEvent;
          try {
            event = JSON.parse(data) as CompanyStreamEvent;
          } catch (err) {
            console.debug("[events] dropping unparseable event", err);
            return;
          }
          handleEvent(event, {
            onAgentReply: onAgentReplyRef.current,
            onTaskEvent: onTaskEventRef.current,
            onRunEvent: onRunEventRef.current,
            onDispatchTerminal: onDispatchTerminalRef.current,
            onWorkspaceEvent: onWorkspaceEventRef.current,
            onTurnEvent: onTurnEventRef.current,
            onWorkflowRunEvent: onWorkflowRunEventRef.current,
            onWorkflowChanged: onWorkflowChangedRef.current,
            onApprovalEvent: onApprovalEventRef.current,
            onResync: recover,
          });
        },
        onError: ({ reconnecting }) => {
          opened = false;
          startReconciliation();
          console.debug("[events] stream error", { url, reconnecting });
        },
      });
    } catch (err) {
      clearTimeout(openDeadline);
      startReconciliation();
      console.debug("[events] stream unavailable, falling back to poll", err);
      return () => {
        if (reconcileTimer) clearInterval(reconcileTimer);
      };
    }
    console.debug("[events] connecting", { url });

    return () => {
      clearTimeout(openDeadline);
      if (reconcileTimer) clearInterval(reconcileTimer);
      console.debug("[events] disconnecting", { url });
      unsubscribe();
    };
  }, [client, company]);
}

/**
 * Routes one parsed event to its toast / transcript side effect.
 *
 * Exported for tests. This function has been bitten three times by a frame the
 * host was already sending falling through to `default:` and vanishing with
 * nothing to debug (#464 for the board, #371 for the canvas, #384 for the
 * picker), so which subscriber a type reaches is worth pinning directly rather
 * than only through a mounted hook.
 */
export function handleEvent(
  event: CompanyStreamEvent,
  subscribers: Subscribers,
): void {
  const {
    onAgentReply,
    onTaskEvent,
    onRunEvent,
    onDispatchTerminal,
    onWorkspaceEvent,
    onTurnEvent,
    onWorkflowRunEvent,
    onWorkflowChanged,
    onApprovalEvent,
    onResync,
  } = subscribers;
  switch (event.type) {
    // A gap means incremental state is no longer trustworthy. It is structural
    // rather than an attention event, so recovery is deliberately toast-free.
    case "stream_gap":
      void onResync?.();
      break;
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
      onTaskEvent?.(event);
      break;
    // Issue #1015, and **no toast** for the reason the board frames above raise
    // none, more strongly if anything: this fires several times per attempt —
    // mint, start, settle — on every card and every chat turn. Toasting those
    // would train the operator to dismiss the toasts that matter. The attempt
    // row moving on the screen IS the notification.
    case "run_status_changed":
      onRunEvent?.(event);
      break;
    // Issue #377: the settle is two facts at once, so it reaches two
    // subscribers. The board gets its tick, exactly as it has since #464 — a
    // settle moves a card between columns. The channel the card was raised in
    // gets a marker saying it settled and where, which is the fact that was
    // missing: a reader watching only the relay prose could not tell a card
    // that parked in `paused` from one that finished.
    //
    // Still **no toast**, and for the reason written above the board arm: this
    // fires on every settle, and the marker appearing in the channel IS the
    // notification. A toast on top would be the second notification for one
    // action that #379 argues against.
    case "desk_task_completed":
      onTaskEvent?.(event);
      onDispatchTerminal?.(event);
      break;
    // Issue #327, and **no toast** for the same reason the board frames above
    // raise none — more strongly, if anything. A workspace write is not an
    // attention signal: an autosaving editor fires one per settled keystroke
    // burst, and every published deliverable adds more. Toasting those would
    // train the operator to dismiss the toasts that do matter. The note
    // appearing, changing or vanishing in the tree IS the notification.
    case "workspace_changed":
      onWorkspaceEvent?.(event);
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
      // #971: an expiry is a resolution — a default-deny on silence — but it is
      // not a decision anybody made. Saying "Approval denied" for one told the
      // operator they had declined something they never saw, and shortening the
      // deadline to 24 hours turns that from rare into routine. So the
      // host-resolved case gets its own words, and only that case: `automatic`
      // is set solely on the System arm, so a person's own decision reads
      // exactly as it did before.
      if (event.automatic) {
        toast("Approval expired", {
          description:
            "It passed its deadline with no decision, so it was declined.",
        });
      } else {
        toast(
          event.verdict === "approve" ? "Approval granted" : "Approval denied",
          {
            description: "An approval was just resolved.",
          },
        );
      }
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
    // Issue #371/#382: progress frames route to the same subscriber the finished
    // event does, and deliberately raise NO toast. Progress is not an attention
    // signal — a six-node run would fire a dozen-plus toasts and train the
    // operator to dismiss the one that matters. The canvas is where this belongs.
    //
    // These arms exist because this file has already been bitten by events
    // falling through to `default:` and vanishing; a new event type without an
    // arm here is silently dropped, with nothing to debug. `workflow_node_started`
    // (#382) is the newest such frame — it lights a node up on the canvas.
    case "workflow_run_started":
    case "workflow_node_started":
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
    // Issue #276: arming and pausing belong here too. It is not an authoring
    // change, but it changes what the picker must render, and the question the
    // subscriber answers — "re-read the list" — is the same one. A workflow
    // paused by another session, or disarmed by the host on someone else's
    // edit, used to stay showing as armed until a reload.
    case "workflow_enabled_changed":
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
