// `#/tasks/<id>` — the card detail, and all that is left of the Tasks page.
//
// The board screen used to render `TaskDetailView` in place of itself whenever
// the address carried a card id, so the detail's only mount point was inside a
// page that has now been retired (issue #1140). This is that mount point, moved
// somewhere the deletion could not take it.
//
// # What it owns, and what it no longer has to
//
// **The id is not its problem any more.** The old screen kept the card id in
// state and re-read it on every `hashchange`, because it was also a board and
// the shell had no idea a sub-page was open. The shell's router already hands
// the second segment down, so the id arrives as a prop and changes when the
// address changes — one fewer listener, and no way for the two to disagree.
//
// **The focus still is.** `#/tasks/<id>?artifact=…&v=…` and `?run=…` (issue
// #339) live in the *query* half of the hash, which the router deliberately
// strips: it routes on path segments. So this screen reads the focus itself and
// follows it, which is what makes a link to one attempt's trace land on the
// Attempts tab even when the detail is already open on that same card.

import { useEffect, useState } from "react";

import type { OpenCompanyClient } from "@/api/client";
import type { ApprovalSummary, GrantScope, Verdict } from "@/api/types";
import {
  readTaskFocus,
  taskTabHref,
  type TaskFocus,
  type TaskTab,
} from "@/lib/task-output";
import { LEDGER_VIEW_PARAM, readLedgerViewMode } from "@/hooks/use-ledger-view-mode";
import { withHostParam } from "@/hooks/use-host-route";
import type { DecidedApproval } from "@/views/chat/model";
import { TaskDetailView } from "@/views/TaskDetailView";

export function TaskDetailRoute({
  client,
  company,
  taskId,
  attemptEventTick,
  parked,
  deciding,
  decided,
  failed,
  onDecide,
  onOpenThread,
  onLeave,
}: {
  client: OpenCompanyClient;
  company: string | null;
  /** The card named by the address. Never empty — the shell rewrites `#/tasks`. */
  taskId: string;
  /** Bumped on every `run_status_changed` (issue #1015), for the live attempt. */
  attemptEventTick?: number;
  /** The company's parked approvals, so a waiting card can name what it waits on. */
  parked?: readonly ApprovalSummary[];
  /** The console-wide decision state the detail decides through (#1891) —
   *  passed straight down, exactly as `parked` is. */
  deciding?: ReadonlyMap<string, Verdict>;
  decided?: Readonly<Record<string, DecidedApproval>>;
  failed?: Record<string, string>;
  onDecide?: (approval: ApprovalSummary, verdict: Verdict, scope: GrantScope) => void;
  /** Opens the chat thread this card was created from (issue #246). */
  onOpenThread?: (threadId: string) => void;
  /** Where Back, and a deleted card, go: the board, which lives in Ledgers. */
  onLeave: () => void;
}) {
  const [focus, setFocus] = useState<TaskFocus>(() =>
    readTaskFocus(window.location.hash),
  );

  useEffect(() => {
    const onHash = () => setFocus(readTaskFocus(window.location.hash));
    window.addEventListener("hashchange", onHash);
    return () => window.removeEventListener("hashchange", onHash);
  }, []);

  return (
    <TaskDetailView
      client={client}
      company={company}
      taskId={taskId}
      attemptEventTick={attemptEventTick}
      parked={parked}
      deciding={deciding}
      decided={decided}
      failed={failed}
      onDecide={onDecide}
      focus={focus}
      onTabChange={(tab: TaskTab) => {
        // A tab is part of the task detail's current place, but a succession
        // of clicks is not a succession of browser destinations. Replacing the
        // current entry means Back still returns to the board and Forward
        // returns to the tab the operator was using there.
        const next = taskTabHref(window.location.hash, tab);
        if (next === window.location.hash) return;
        window.history.replaceState(null, "", next);
        // replaceState does not emit hashchange, so follow it locally.
        setFocus(readTaskFocus(next));
      }}
      onBack={onLeave}
      onNavigate={(id) => {
        const view = readLedgerViewMode();
        window.location.hash = withHostParam(`tasks/${encodeURIComponent(id)}`, {
          [LEDGER_VIEW_PARAM]: view === "list" ? "list" : null,
        });
      }}
      onOpenThread={onOpenThread}
      // Both of these used to hand a card back to the board rendered beside
      // this screen, which held its own copy of the row. There is no such
      // sibling now: the board is the `tasks` ledger, it re-reads from the host,
      // and it is not mounted while this screen is. So a save reconciles
      // nothing here — the board reads the change when the operator returns to
      // it — and a delete leaves for the board rather than sitting on the
      // detail of a card that no longer exists.
      onSaved={() => {}}
      onDeleted={onLeave}
    />
  );
}
