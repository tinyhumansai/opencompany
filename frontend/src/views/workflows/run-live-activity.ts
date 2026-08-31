// The live-streaming delta for the run-trace sheet (issue #1702).
//
// Issue #596 gave the sheet a durable, SNAPSHOT-only view of a finished run: the
// per-node output text, fetched once the run settles. Nothing showed a workflow
// agent node's tool calls *as they happened* — the sheet's in-flight state was a
// single "Still running…" line.
//
// This module folds the transient `tool_call`/`tool_result` frames the host now
// tags with `workflowRunId`/`nodeId` (see `src/turn_stream.rs`) into a per-node
// tool timeline the sheet can render live. It is a pure reducer plus the hook
// that drives it off the existing SSE subscription, kept out of the component so
// the fold — where the dedup rule lives — is unit-testable without a DOM.

import { useEffect, useMemo, useState } from "react";

import type { OpenCompanyClient } from "@/api/client";
import type { CompanyStreamEvent } from "@/hooks/use-events";

/** One tool call in a node's live timeline. A start and its matching result are
 * ONE row, not two: they share a `toolCallId`, so the result updates the row the
 * start created in place (running → ok/error). */
export interface LiveToolRow {
  /** The dedup key: the frame's `toolCallId`, or `seq:<n>` when a frame carries
   * none, so a replayed frame (a reconnect, a test) rewrites its row rather than
   * appending a second one. */
  key: string;
  /** The start frame's dense per-turn sequence, so rows order by when the call
   * began and a later result cannot jump the row to the end. */
  seq: number;
  label: string;
  detail?: string;
  /** What came back — a success's shape summary or a failure's cause, on the
   * result only. Carried for the same reason `detail` is, and it is the only
   * descriptive payload an **ACP** node's completion has: an ACP tool call puts
   * no arguments on the wire, so there is nothing to derive a `detail` from and
   * a dropped `result` leaves the row saying only that something finished. */
  result?: string;
  /** `running` on a start; `ok` / `error` / `awaiting_approval` on the result. */
  status: string;
  /** Wall-clock the completed call took, on the result only. */
  elapsedMs?: number;
}

/** The fold's internal shape: node id → (dedup key → row). Keyed maps rather
 * than arrays so both the start/result merge and the replay dedup are O(1) and
 * cannot double a row. */
export type LiveActivityState = Record<string, Record<string, LiveToolRow>>;

/** A node's live rows, ready to render — ordered by the call's start seq. */
export interface LiveNode {
  nodeId: string;
  rows: LiveToolRow[];
}

/** When a frame carries no `nodeId` (a graph compiled before node identity, or a
 * hand-built test request), its rows still have somewhere honest to land rather
 * than being dropped. */
const UNATTRIBUTED_NODE = "—";

/**
 * Folds one live frame into the per-node activity state.
 *
 * A no-op for any frame that is not a `tool_call`/`tool_result` for THIS run —
 * the caller passes every frame on the company feed, so the run filter lives
 * here rather than being trusted to the subscription. A `tool_result` merges
 * onto the row its `tool_call` created (same `toolCallId`), which is the whole
 * dedup: the two frames are one row, and a replay of either just rewrites it.
 */
export function foldLiveFrame(
  state: LiveActivityState,
  frame: CompanyStreamEvent,
  workflowRunId: string,
): LiveActivityState {
  if (frame.type !== "tool_call" && frame.type !== "tool_result") return state;
  if (frame.workflowRunId !== workflowRunId) return state;

  const nodeId = frame.nodeId ?? UNATTRIBUTED_NODE;
  const key = frame.toolCallId ?? `seq:${frame.seq}`;
  const prev = state[nodeId]?.[key];
  const isResult = frame.type === "tool_result";

  const row: LiveToolRow = {
    key,
    // A start's seq is the row's canonical position: a result arriving after
    // its start (the normal order) inherits the start's seq, while a start
    // arriving after its result (broadcast reordering) uses its OWN seq rather
    // than inheriting the later result's — so the row sits where the call began.
    seq: isResult ? (prev?.seq ?? frame.seq) : frame.seq,
    label: frame.label ?? prev?.label ?? "Tool call",
    // The result carries detail/elapsed; a start does not, so never let a
    // start frame blank a value the result already wrote (out-of-order arrival).
    detail: isResult ? (frame.detail ?? prev?.detail) : prev?.detail,
    result: isResult ? (frame.result ?? prev?.result) : prev?.result,
    // A result's status is terminal; a start's is only ever "running". When a
    // start arrives late, keep the status the result already wrote so a
    // completed call is not shown as running again.
    status: isResult
      ? (frame.status ?? prev?.status ?? "running")
      : (prev?.status ?? frame.status ?? "running"),
    elapsedMs: isResult ? (frame.elapsedMs ?? prev?.elapsedMs) : prev?.elapsedMs,
  };

  return {
    ...state,
    [nodeId]: { ...(state[nodeId] ?? {}), [key]: row },
  };
}

/** Projects the keyed fold into render-ready nodes: rows sorted by start seq,
 * nodes sorted by their earliest row so a node that started first sits first. */
export function liveNodes(state: LiveActivityState): LiveNode[] {
  return Object.entries(state)
    .map(([nodeId, rows]) => ({
      nodeId,
      rows: Object.values(rows).sort((a, b) => a.seq - b.seq),
    }))
    .filter((node) => node.rows.length > 0)
    .sort((a, b) => a.rows[0].seq - b.rows[0].seq);
}

/**
 * Subscribes to the company SSE feed and folds this run's live tool frames into
 * a per-node timeline, for the run-trace sheet to render while a run executes
 * (issue #1702).
 *
 * `active` gates the subscription — the sheet passes `isRunning(run)`, so the
 * stream is open only while the run is in flight. When it settles the hook stops
 * folding but KEEPS what it collected, so the live trace stays on screen beside
 * the durable snapshot rather than vanishing the instant the run finishes. The
 * collected rows reset only when `workflowRunId` changes (a different run opened
 * in the sheet).
 */
export function useLiveNodeActivity(
  client: OpenCompanyClient,
  company: string | null,
  workflowRunId: string | null,
  active: boolean,
): LiveNode[] {
  const [state, setState] = useState<LiveActivityState>({});

  // A new run in the sheet starts from an empty timeline; a settle does not.
  useEffect(() => {
    setState({});
  }, [workflowRunId]);

  useEffect(() => {
    if (!active || !workflowRunId) return;
    let unsubscribe: (() => void) | undefined;
    try {
      unsubscribe = client.subscribeToEvents(company, {
        onMessage: (data) => {
          let frame: CompanyStreamEvent;
          try {
            frame = JSON.parse(data) as CompanyStreamEvent;
          } catch {
            return;
          }
          if (
            (frame.type === "tool_call" || frame.type === "tool_result") &&
            frame.workflowRunId === workflowRunId
          ) {
            setState((prev) => foldLiveFrame(prev, frame, workflowRunId));
          }
        },
      });
    } catch {
      // Streaming unavailable (no `EventSource`, a malformed URL, …): the
      // contract lets `subscribeToEvents` throw synchronously, and the sheet
      // must simply omit the live half and keep the durable snapshot rather
      // than surface a React effect error and blank the view.
      return;
    }
    return unsubscribe;
  }, [client, company, workflowRunId, active]);

  return useMemo(() => liveNodes(state), [state]);
}
