import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { Check, Clock, Loader2, ShieldAlert, ShieldCheck, X } from "lucide-react";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import { toast } from "sonner";

import type { OpenCompanyClient } from "@/api/client";
import {
  ApiError,
  type ApprovalSummary,
  GRANT_DURATIONS,
  type GrantScope,
  type StandingGrant,
  type Verdict,
} from "@/api/types";
import {
  ApprovalHeadline,
  DeclineScopeControl,
  ApprovalMeta,
  ApprovalPayload,
  ApprovalScopeControl,
  useAskerNames,
  useApprovalThreadLinks,
} from "@/components/approval-card";
import { PageHeader } from "@/components/page-header";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { useApprovalDeadline } from "@/hooks/use-approval-deadline";
import type { CompanyFeed } from "@/hooks/use-company";
import { useStableList } from "@/hooks/use-stable-list";
import {
  approvedByRuntimeLine,
  approvedLine,
  batchPositions,
  staleDecisionLine,
} from "@/lib/approval-wording";
import { approvalsByDeadline } from "@/lib/approval-order";
import {
  approvalSummary,
  decisionLabel,
  grantHeadline,
  grantSubject,
  timeAgo,
  toolAction,
  untilLabel,
} from "@/lib/language";
import { approvalsForTask } from "@/lib/task-approvals";
import { startVisiblePolling } from "@/lib/visible-poll";
import { isRecord, parseNodeMessages } from "@/views/workflows/run-output";

/**
 * Below this, a lost connection cannot have outlived the fast part of a resolve.
 *
 * Generous on purpose: the fast part is three local writes, so anything past a
 * second or two of *waiting* was waiting on the agent turn. The number only has
 * to separate "failed before it started" from "failed after it was underway".
 */
const RESOLVE_SLOW_ENOUGH_MS = 2_000;

/**
 * Whether the host may have carried out this request despite the error (#380).
 *
 * The console genuinely cannot tell "the verdict never landed" from "the
 * verdict landed and the continuation timed out" — but it can tell who
 * answered, and roughly how long it waited, and that is enough to stop lying in
 * the cases that actually occur.
 *
 * **The host answered in its own envelope** (`fromHost`): it considered the
 * request and refused, so nothing landed, whatever the status was. This is the
 * arm that matters most for correctness — the host returns 503 while quiescing
 * and 502 on an upstream transport failure, so a status-only check would read
 * those clean refusals as timeouts and tell the operator a decision was
 * recorded when it provably was not.
 *
 * **A proxy answered** (502/503/504 with no envelope): on a hosted tenant that
 * is a reverse proxy that could not get a reply out of an upstream it had
 * already handed the request to. The verdict very probably landed.
 *
 * **Nothing answered**: only evidence of a slow turn if it was actually slow.
 * An instant rejection is an offline browser or a refused connection, where the
 * request never reached the host — and saying "your decision was recorded"
 * there would be a fresh lie in place of the one being fixed. Hence the
 * elapsed-time floor: the inference is "recorded *before* the delay", so it
 * needs a delay to stand on.
 *
 * Either way `feed.refresh()` at the call site settles it, since the host drops
 * the approval from the queue before anything slow happens.
 */
function mayHaveLanded(err: unknown, elapsedMs: number): boolean {
  if (!(err instanceof ApiError)) return false;
  if (err.fromHost) return false;
  if (err.status >= 502) return true;
  return err.code === "network_error" && elapsedMs >= RESOLVE_SLOW_ENOUGH_MS;
}

interface Props {
  client: OpenCompanyClient;
  company: string | null;
  feed: CompanyFeed;
  /**
   * `#/approvals/<taskId>` — narrow the queue to one board card (issue #883).
   *
   * Arrives unvalidated, as every hash sub-page does. An id matching nothing
   * parked is a legitimate state rather than an error: the operator followed a
   * blocked card's Review link and the last of its approvals was decided on the
   * way, which is the flow working. It renders as "this card is clear" with a
   * way back to the whole queue, never as the generic "nothing needs you" —
   * those are different facts and only one of them is about the whole company.
   */
  sub?: string | null;
  onResolved: (systemLine: string) => void;
  onGoToConversation: () => void;
  /** Host thread id → console channel id, owned by the shell's chat hydration. */
  chatChannelByThread?: Readonly<Record<string, string>>;
  /**
   * Called the instant a decide click starts, before the network call
   * (issue #1211) — so the shell can mark this approval as "this tab decided
   * it" before the SSE echo of the resolution has a chance to race ahead of
   * the awaited response and arrive first.
   */
  onDecideStart?: (approvalId: string) => void;
}

/** The approvals inbox: the few things the company parked for the operator. */
export function ApprovalsView({
  client,
  company,
  feed,
  sub,
  onResolved,
  onGoToConversation,
  chatChannelByThread,
  onDecideStart,
}: Props) {
  const approvalTtlHours = useApprovalDeadline(client, company);
  // Issue #373: in-flight state is per approval, not a single module-wide slot.
  //
  // Approving is not a quick write — the host mints a grant and re-dispatches
  // the agent, holding the POST open for a whole turn (#243) — so two decisions
  // being in flight at once is a legitimate state the operator can reach, and
  // one the host already handles by serialising them behind its per-company
  // lock. The old `string | null` could not represent it, which is why deciding
  // one card greyed out every other card on the screen until a hard reload.
  //
  // A map rather than a set of ids because the verdict has to survive the wait:
  // an approve and a decline are different promises to the operator ("the agent
  // is doing it" vs "recorded"), and the card says which one it is waiting on.
  const [inFlight, setInFlight] = useState<ReadonlyMap<string, Verdict>>(
    () => new Map(),
  );
  // Issue #1805: which cards have an in-flight deadline extension. Separate from
  // `inFlight` (a verdict) — extending is not a decision, so it neither blocks
  // nor is blocked by one being recorded; it only guards a double-press on its
  // own button.
  const [extending, setExtending] = useState<ReadonlySet<string>>(
    () => new Set(),
  );
  const { approvals, now, queue } = feed;
  /**
   * The card the queue is narrowed to, or `null` for the whole queue (#883).
   *
   * Decoded because the shell percent-encodes the id into the hash; a malformed
   * escape throws `URIError`, and a broken link must fall back to the full queue
   * rather than blank the page.
   */
  const focusTaskId = useMemo(() => {
    if (!sub) return null;
    try {
      return decodeURIComponent(sub);
    } catch {
      return null;
    }
  }, [sub]);
  /**
   * The rows on screen. Every other derivation below — the batch totals, the
   * asker names, the decide path — deliberately stays on the **full** queue:
   * a batch's "1 of 3 from this turn" counts what the turn parked, and filtering
   * the count with the view would turn a batch of three into a batch of one and
   * tell the operator the opposite of the truth.
   */
  const visible = useMemo(
    () =>
      focusTaskId === null
        ? approvals
        : approvalsForTask(approvals, focusTaskId),
    [approvals, focusTaskId],
  );
  /**
   * The queue's priority order, before its pointer-stable snapshot (#1427).
   *
   * A deadline is the one ordering signal the host enforces without operator
   * input, so the earliest one must not be stranded beneath arbitrary response
   * order. Sorting before `useStableList` keeps #1414's guarantee intact: a
   * polling response can update the intended order, but cannot rearrange cards
   * while the operator is pointing at or tabbing through the queue.
   */
  const orderedVisible = useMemo(() => approvalsByDeadline(visible), [visible]);
  /**
   * The same rows, but in a pointer-stable order (#1414).
   *
   * A poll swaps `visible` wholesale every 5s, and mapping that straight to
   * cards let the queue reflow under the operator's pointer — a card removed
   * above the pointer slid the next card's Approve button under an in-flight
   * click. `useStableList` holds the rendered order (and holds removals) for as
   * long as the pointer is over the queue or focus is inside it, then
   * reconciles to the latest poll the moment the operator moves away. Every
   * branch below reads `rows` rather than `visible` so the count, the empty
   * state and the list all agree on the one frozen view.
   */
  const {
    items: rows,
    containerProps: queueHold,
    holding,
  } = useStableList(orderedVisible);
  // Relative timestamps above the queue can wrap at a formatting boundary just
  // like roster names or grant membership. Keep that clock stable for the whole
  // interaction region, then let the next render use the current feed time.
  const heldNow = useRef(now);
  if (!holding) heldNow.current = now;
  const regionNow = holding ? heldNow.current : now;
  const askerNames = useAskerNames(client, company, approvals, holding);
  const threadLinks = useApprovalThreadLinks(client, company, rows, holding);
  const { grants, granterNames, refreshGrants } = useStandingGrants(
    client,
    company,
    holding,
  );
  /**
   * How many rows each turn's batch still has waiting (#842).
   *
   * The page stays **itemised** — one row per gated call, each independently
   * approvable, exactly as `Standing permissions` below lists one revocable row
   * per grant. This is the one thing it borrows from the conversation's
   * consolidated card: a row says how many others were asked for alongside it,
   * so an operator who arrives here from the toast can tell "this is one of
   * three from one turn" from "these are three unrelated requests" — which is
   * the difference between deciding the batch and deciding a queue.
   *
   * Counted over what is still pending rather than over the whole batch, so the
   * number shrinks as rows are decided instead of promising a fourth row that
   * has already been signed off.
   *
   * Both halves of "N of M" come from one walk of that pending list (#1289): a
   * per-card `index` as well as the batch `total`, so a two-card turn reads
   * "1 of 2" then "2 of 2" rather than the hardcoded "1 of 2" twice, and a
   * focus-narrowed view cannot count the position over a subset and the total
   * over the whole.
   */
  const batchPosLive = useMemo(() => batchPositions(approvals), [approvals]);
  // The queue hold freezes `rows`, so a poll that expires or decides one card
  // of a batch must not renumber the cards the operator is looking at: without
  // the freeze a removed frozen card falls back to "1 of 1" (the line vanishes)
  // and its surviving siblings get shorter counts — either can rewrap a card
  // above the targeted control. Snapshot the map with the hold, reconcile the
  // moment it releases (#1593).
  const batchPosHeld = useRef(batchPosLive);
  if (!holding) batchPosHeld.current = batchPosLive;
  const batchPos = holding ? batchPosHeld.current : batchPosLive;

  const markInFlight = (id: string, verdict: Verdict | null) =>
    setInFlight((prev) => {
      const next = new Map(prev);
      if (verdict) next.set(id, verdict);
      else next.delete(id);
      return next;
    });

  async function decide(
    a: ApprovalSummary,
    verdict: Verdict,
    scope: GrantScope,
  ) {
    // Per-row guard: only a double-press on THIS card is ignored. The global
    // early return that used to live here made every other card inert too.
    if (inFlight.has(a.id)) return;
    onDecideStart?.(a.id);
    markInFlight(a.id, verdict);
    const startedAt = Date.now();
    try {
      const answer = await client.resolveApproval(
        a.id,
        verdict,
        undefined,
        company,
        { scope },
      );
      // Issue #243: approving no longer just records a verdict — it hands the
      // agent a single-use grant and re-dispatches it to make the call. The old
      // "Approved: …" read as "done", which was the exact lie that made the
      // missing re-dispatch invisible: the operator saw a success toast for work
      // that had silently dead-ended. Say what is actually happening instead.
      // Declining IS terminal, so its wording is unchanged.
      // Say which scope actually landed. "Approved" alone would read the same
      // for a one-off and for a week-long permission, and the operator has to be
      // able to tell those apart from the confirmation they just got.
      //
      // …and only say "the teammate" when there IS one (#395). A card with no
      // `agent` is one the runtime performs itself — a paused workflow gate, a
      // cold-recipient report — and naming a teammate there is the same shape of
      // small lie the wording above exists to remove. The work is still in
      // flight either way, so both halves say so; only the actor changes.
      // Issue #561: what happens next is the host's answer, not this view's
      // guess. A turn continues once, when the last decision it parked lands,
      // so approving one of several releases nothing — and saying otherwise is
      // the one part of this flow that actively misleads.
      const stillAwaiting =
        "stillAwaiting" in answer ? answer.stillAwaiting : undefined;
      // Issue #1449, and it comes FIRST because everything below it is written
      // for a decision that actually happened. The host answers `200` to a click
      // on a card whose deadline has passed — it has to, nothing failed — and
      // then default-denies it. Read that way, the wording below was a green
      // success line over work the host had just refused, and the operator's
      // only next signal was the work silently not happening.
      //
      // `null` means the host said `settled`, or is too old to say. Both keep
      // the pre-#1449 wording: guessing is the defect, in either direction.
      const stale = staleDecisionLine(
        "outcome" in answer ? answer.outcome : undefined,
        approvalSummary(a),
      );
      if (stale) {
        onResolved(stale);
        // Neither success nor error, exactly as the #380 timeout line is
        // neither: the request was answered correctly and the answer is that
        // there was no decision left to make. Still exactly one toast for this
        // click (#1211) — the SSE echo for the id is suppressed by
        // `onDecideStart` above, whichever verdict the host ends up appending.
        toast.info(stale);
        // The queue is the reconciliation: this card is gone from the host's
        // parked set either way, so a refresh drops it.
        void feed.refresh();
        return;
      }
      const line =
        verdict !== "approve"
          ? `Declined: ${approvalSummary(a)}`
          : scope.kind === "tool"
            ? `Approved — ${toolAction(a.kind).toLowerCase()} won't ask again until this permission expires. Take it back under Standing permissions.`
            : a.agent
              ? approvedLine(stillAwaiting, approvalSummary(a))
              : approvedByRuntimeLine(stillAwaiting, approvalSummary(a));
      onResolved(line);
      toast.success(line);
      // The agent's reply arrives as a journaled `AgentReply` on its own thread,
      // so no extra plumbing is needed here — the existing feed refresh plus the
      // per-agent DM thread (#151) surface it.
      void feed.refresh();
      // A tool-scoped approve minted a permission, so the list below is stale.
      if (scope.kind === "tool") void refreshGrants();
    } catch (err) {
      // Issue #380: a failed request is not the same as a failed decision.
      //
      // The host resolves in four steps — drop the approval from the parked
      // queue, journal the verdict durably, mint the grant, then run a whole
      // follow-up agent turn — and only the last one is slow. So when the
      // answer is lost in transit it is lost *after* the verdict is durable,
      // and "Couldn't record your decision" is a lie that invites the one
      // response that cannot help: approving again.
      if (mayHaveLanded(err, Date.now() - startedAt)) {
        const line =
          verdict === "approve"
            ? "Approved — the host didn't answer in time, but your decision was recorded. The teammate may still be working; no need to approve again."
            : "Declined — the host didn't answer in time, but your decision was recorded. No need to decline again.";
        onResolved(line);
        // Neither a success nor an error: the verdict is durable, the
        // continuation is unknown. A green tick would overclaim and a red
        // cross is the bug being fixed.
        toast.info(line);
        // The reconciliation, and the reason the copy above can be this
        // confident: the host removes the approval from the queue in step one,
        // so a refresh either drops this card — proving the verdict landed —
        // or leaves it, showing the operator a decision that still needs
        // making. The queue is the answer the response body never delivered.
        void feed.refresh();
      } else {
        const msg =
          err instanceof ApiError ? err.message : "something went wrong";
        onResolved(`Couldn't record your decision — ${msg}`);
        toast.error(`Couldn't record your decision — ${msg}`);
      }
    } finally {
      // Unconditional, and keyed on the id rather than clearing a single slot:
      // the feed refreshes on its own schedule and routinely drops the decided
      // row while its request is still open, so the flag has to be removable
      // whether or not the row it belongs to still exists. Deleting a key that
      // is already gone is a no-op, which is the point.
      markInFlight(a.id, null);
    }
  }

  // Issue #1805: push a card's deadline out so a stalled run does not
  // default-deny before someone can decide it. Not a verdict — the card stays
  // parked and decidable — so it runs on its own in-flight set and refreshes the
  // feed to redraw the countdown from the new deadline the host reports.
  async function extendDeadline(a: ApprovalSummary) {
    if (extending.has(a.id)) return;
    setExtending((prev) => new Set(prev).add(a.id));
    try {
      await client.extendApproval(a.id, company);
      const line = `Extended the deadline for ${approvalSummary(a)}.`;
      onResolved(line);
      toast.success(line);
      // The new deadline is on the host now; a refresh redraws the countdown.
      void feed.refresh();
    } catch (err) {
      // A 404 means it is no longer parked (already decided or expired
      // elsewhere) — reconcile by refreshing rather than reporting a failure.
      if (err instanceof ApiError && err.status === 404) {
        void feed.refresh();
      } else {
        const msg =
          err instanceof ApiError ? err.message : "something went wrong";
        toast.error(`Couldn't extend the deadline — ${msg}`);
      }
    } finally {
      setExtending((prev) => {
        const next = new Set(prev);
        next.delete(a.id);
        return next;
      });
    }
  }

  // Bulk decisions for the whole visible queue.
  //
  // Client-side and sequential on purpose, rather than a new batch route: each
  // call goes through the existing, validated resolve path, and the host already
  // serialises decisions behind its per-company lock. Sending them one at a time
  // respects that lock, reuses the per-row drop-safety, and keeps the view honest
  // if one fails partway - the rows that landed drop out, the rest stay.
  //
  // Scope is always `once`: a bulk action never mints a standing grant - that
  // stays a deliberate per-tool choice.
  //
  // Approve is not terminal: each approval resumes the agent, so approving many
  // at once starts several follow-up turns. Hence the confirm copy. Decline is
  // terminal and lighter.
  const [bulkInFlight, setBulkInFlight] = useState(false);

  async function decideAll(verdict: Verdict) {
    if (bulkInFlight || rows.length === 0) return;
    const n = rows.length;
    const question =
      verdict === "approve"
        ? `Approve ${n} ${n === 1 ? "request" : "requests"}? Each approval resumes the teammate, so this may start several tasks at once.`
        : `Decline ${n} ${n === 1 ? "request" : "requests"}? This is final; the work behind them moves on without them.`;
    if (!window.confirm(question)) return;
    setBulkInFlight(true);
    try {
      for (const a of [...rows]) {
        if (inFlight.has(a.id)) continue;
        await decide(a, verdict, { kind: "once" });
      }
    } finally {
      setBulkInFlight(false);
    }
  }

  return (
    <div className="flex-1 overflow-y-auto">
      <div className="mx-auto w-full max-w-3xl px-4 py-6">
        {/* The queue's own count heading below only renders once loaded, so
            it can't be the page's one `h1` — this stays present through
            loading, error and empty states alike (issue #1221). */}
        <PageHeader hidden title="Approvals" />
        {/* Issue #883: the filter says so, and offers the way out of itself.
            A narrowed queue that looked identical to the whole one would make a
            decided-elsewhere approval look like it had vanished. */}
        {focusTaskId !== null && (
          <div className="mb-4 flex items-center justify-between gap-3 rounded-lg border bg-muted/40 px-3 py-2 text-xs">
            <span className="min-w-0 text-muted-foreground">
              Showing only what one board card is waiting on.
            </span>
            <a
              href="#/approvals"
              className="shrink-0 font-medium underline-offset-2 hover:underline"
            >
              Show all
            </a>
          </div>
        )}
        {/* #1427: permissions are a separate operator task, not the last queue
            row. Keeping them before the pending list makes revocation reachable
            even when a backlog is several screens long. The queue's hold props
            wrap the whole region — permissions and rows together — so the
            section stays still while an operator is acting on it. */}
        <div {...queueHold}>
          <StandingPermissions
            grants={grants}
            now={regionNow}
            askerNames={askerNames}
            granterNames={granterNames}
            onRevoke={async (id) => {
              try {
                await client.revokeGrant(id, company);
                toast.success(
                  "Permission revoked — this tool will ask again from its next call.",
                );
              } catch (err) {
                // A 404 means it was already gone (revoked elsewhere, or expired).
                // The operator's intent is satisfied either way, so this is not an
                // error to them — only a stale list, which the refresh below fixes.
                if (err instanceof ApiError && err.status === 404) {
                  toast.info("That permission was already gone.");
                } else {
                  const msg =
                    err instanceof ApiError
                      ? err.message
                      : "something went wrong";
                  toast.error(`Couldn't revoke it — ${msg}`);
                  throw err;
                }
              } finally {
                void refreshGrants();
              }
            }}
          />

          {/* Issue #1229: "nothing parked" and "we could not read what is parked"
              are different facts, and only one of them is an instruction to stop
              looking. The queue's own load state decides which is on screen —
              `approvals` being empty cannot, because it is empty in both cases.
              A queue that has been read once keeps its rows through a later
              failure, so this branch is only ever the cold path. */}
          {queue !== "ready" && approvals.length === 0 ? (
            queue === "loading" ? (
              <LoadingApprovals />
            ) : (
              <UnreadableApprovals onRetry={() => void feed.refresh()} />
            )
          ) : rows.length === 0 ? (
            focusTaskId !== null ? (
              <ClearedForTask />
            ) : (
              <EmptyApprovals onGoToConversation={onGoToConversation} />
            )
          ) : (
            <>
              {/* #1427: the count and deadline rule orient every viewport, not
                  only the first one. The opaque background makes the header a
                  real reading boundary over cards that scroll beneath it. */}
              <div className="sticky top-0 z-10 -mx-4 mb-3 border-b bg-background px-4 py-3">
                <div className="flex items-baseline justify-between gap-3">
                  <h2 className="text-sm font-medium text-muted-foreground">
                    {rows.length === 1
                      ? "1 thing needs your approval"
                      : `${rows.length} things need your approval`}
                  </h2>
                  {rows.length > 1 && (
                    <div className="flex shrink-0 items-center gap-2 self-center">
                      <Button
                        variant="outline"
                        size="sm"
                        disabled={bulkInFlight || inFlight.size > 0}
                        onClick={() => void decideAll("deny")}
                      >
                        {bulkInFlight ? (
                          <Loader2 className="size-4 animate-spin" />
                        ) : (
                          <X className="size-4" />
                        )}
                        Decline all
                      </Button>
                      <Button
                        size="sm"
                        disabled={bulkInFlight || inFlight.size > 0}
                        onClick={() => void decideAll("approve")}
                      >
                        {bulkInFlight ? (
                          <Loader2 className="size-4 animate-spin" />
                        ) : (
                          <Check className="size-4" />
                        )}
                        Approve all
                      </Button>
                    </div>
                  )}
                </div>
                {/* #971: nothing may vanish unannounced. Requests now age out on
                    their own, so the queue says so once, up front. Each card
                    carries its own deadline; this is the sentence that stops
                    that deadline being a surprise. */}
                <p className="mt-1 text-xs text-muted-foreground">
                  Each one has a deadline. — {approvalTtlHours} {approvalTtlHours === 1 ? "hour" : "hours"}. Anything still undecided by then is declined on its own, and the work behind it moves on.
                </p>
              </div>
              <div className="flex flex-col gap-3">
                {rows.map((a) => (
                  <ApprovalCard
                    key={a.id}
                    approval={a}
                    now={regionNow}
                    askerNames={askerNames}
                    chatChannelByThread={chatChannelByThread}
                    thread={threadLinks.get(a.id)}
                    deciding={inFlight.get(a.id) ?? null}
                    batchIndex={batchPos.get(a.id)?.index ?? 1}
                    batchTotal={batchPos.get(a.id)?.total ?? 1}
                    onDecide={(verdict, scope) =>
                      void decide(a, verdict, scope)
                    }
                    extending={extending.has(a.id)}
                    onExtend={() => void extendDeadline(a)}
                  />
                ))}
              </div>
            </>
          )}
        </div>
      </div>
    </div>
  );
}

/**
 * The live standing permissions, polled alongside the approvals feed (#374).
 *
 * Its own read rather than a field on the feed: grants change on operator
 * action, not on company activity, so they do not need the feed's cadence — and
 * a host that predates the route 404s, which is caught here so the section
 * simply does not appear rather than breaking the page.
 */
function useStandingGrants(
  client: OpenCompanyClient,
  company: string | null,
  /**
   * True while the queue is interaction-held (#1593): the section renders above
   * the queue, so a grants update that changes it would shift every
   * approve/decline control while the operator is aiming at one. The freshest
   * grants wait out the hold and land the moment it releases — the same
   * hold-and-reconcile shape `useStableList` gives the rows below.
   */
  holding: boolean,
) {
  const [grants, setGrants] = useState<StandingGrant[]>([]);
  // Actor id → what to call them. Without this the row reads "granted by
  // 019fd3d1bddf-000000000005", which is a runtime identifier in front of an
  // operator — the thing the glossary rule forbids, and useless besides.
  const [granterNames, setGranterNames] = useState<Map<string, string>>(
    new Map(),
  );
  // The newest grants while the queue is held. `null` means nothing pending; an
  // empty list is a real answer and must not be mistaken for "nothing arrived".
  const pending = useRef<StandingGrant[] | null>(null);
  // Names can change card height too (fallback id/email → display name), so they
  // use the same hold-and-reconcile path rather than updating above the queue.
  const pendingNames = useRef<Map<string, string> | null>(null);
  // Live is read inside `refreshGrants`, which is memoised and must not close
  // over a stale render — the same ref pattern `useStableList` uses for `live`.
  const holdingRef = useRef(holding);
  holdingRef.current = holding;

  const refreshGrants = useCallback(async () => {
    const next = await client
      .listGrants(company)
      .catch(() => [] as StandingGrant[]);
    if (holdingRef.current) {
      pending.current = next;
      return;
    }
    setGrants(next);
  }, [client, company]);

  // Grants that arrived during a hold render the moment it releases. Without
  // this a grant minted elsewhere — or by the approve that started the hold —
  // stays invisible until the next poll.
  useEffect(() => {
    if (holding) return;
    if (pending.current !== null) {
      const next = pending.current;
      pending.current = null;
      setGrants(next);
    }
    if (pendingNames.current !== null) {
      const next = pendingNames.current;
      pendingNames.current = null;
      setGranterNames(next);
    }
  }, [holding]);

  useEffect(() => {
    let live = true;
    void (async () => {
      const next = await client
        .listGrants(company)
        .catch(() => [] as StandingGrant[]);
      if (live) {
        if (holdingRef.current) pending.current = next;
        else setGrants(next);
      }
    })();
    // Who the granter ids belong to. Two reads, both allowed to fail: the
    // roster is admin-only, so a member sees no names and the row falls back to
    // a neutral phrase rather than to a raw id.
    void (async () => {
      const [me, users] = await Promise.all([
        client
          .get<{ id: string; email: string; displayName?: string }>(
            `${client.scopeFor(company)}/auth/me`,
          )
          .catch(() => null),
        client
          .get<{ id: string; email: string; display_name?: string }[]>(
            `${client.scopeFor(company)}/users`,
          )
          .catch(() => []),
      ]);
      if (!live) return;
      const names = new Map<string, string>();
      for (const u of users) names.set(u.id, u.display_name?.trim() || u.email);
      // "you" outranks the roster name: an operator reading their own audit
      // trail should not have to recognise their own user id or email.
      if (me) names.set(me.id, "you");
      if (holdingRef.current) pendingNames.current = names;
      else setGranterNames(names);
    })();

    // Slow on purpose. Mint and revoke from another browser are only
    // poll-visible in v1 (there is no event for them), and a permission list is
    // not something an operator watches change — a minute is well inside the
    // shortest duration on offer.
    //
    // Gated on visibility since #581. The cadence was never the problem; a tab
    // left open for a week was, and the load-on-visible read means a returning
    // operator sees the current grants at once rather than up to a minute of a
    // list that may already have been revoked elsewhere.
    const dispose = startVisiblePolling(() => void refreshGrants(), 60_000);
    return () => {
      live = false;
      dispose();
    };
  }, [client, company, refreshGrants]);

  return { grants, granterNames, refreshGrants };
}

/**
 * What the operator has opened up, and how to take it back (#374).
 *
 * Renders nothing when there is nothing standing — including against a host
 * that has no grants route at all, which reads back as an empty list. The
 * section appearing is itself the signal that something is open.
 */
export function StandingPermissions({
  grants,
  now,
  askerNames,
  granterNames,
  onRevoke,
}: {
  grants: StandingGrant[];
  now: number;
  askerNames: Map<string, string>;
  /** Actor id → display name; empty when the roster read was not permitted. */
  granterNames: Map<string, string>;
  onRevoke: (id: string) => Promise<void>;
}) {
  // Per row, not one flag for the section — #373's lesson: a single in-flight
  // slot makes deciding one row freeze every other row on the screen.
  const [revoking, setRevoking] = useState<ReadonlySet<string>>(
    () => new Set(),
  );
  if (grants.length === 0) return null;

  const mark = (id: string, busy: boolean) =>
    setRevoking((prev) => {
      const next = new Set(prev);
      if (busy) next.add(id);
      else next.delete(id);
      return next;
    });

  return (
    <section className="mt-8">
      <h2 className="mb-1 text-sm font-medium text-muted-foreground">
        Standing permissions
      </h2>
      <p className="mb-3 text-xs text-muted-foreground">
        Tools you've allowed or blocked without asking each time. Each one
        expires on its own; you can end it sooner.
      </p>
      <div className="flex flex-col gap-2">
        {grants.map((g, index) => {
          const busy = revoking.has(g.id);
          const expired = g.expires_at_millis <= now;
          return (
            <Card key={g.id} size="sm">
              <CardContent className="flex flex-wrap items-center gap-3">
                <div className="min-w-0 flex-1">
                  {/* Phrased, never the raw identifier — the glossary rule. */}
                  <p className="truncate text-sm font-medium">
                    {grantHeadline(g)}
                  </p>
                  <p className="text-xs text-muted-foreground">
                    {grantSubject(g, askerNames)} ·{" "}
                    {expired
                      ? "expired"
                      : `expires ${untilLabel(g.expires_at_millis, now)}`}{" "}
                    · grant {index + 1} of {grants.length} ·
                    {g.verdict === "deny" ? "declined" : "granted"}{" "}
                    {timeAgo(g.at_millis, now)} by{" "}
                    {granterLabel(g, granterNames)}
                  </p>
                </div>
                <Button
                  variant="outline"
                  size="sm"
                  /* The subject, not just the grant: two teammates holding the
                     same tool and scope read identically in grantHeadline — and
                     a workflow grant carries no agent at all — so button-only
                     navigation would hear identical "Remove" buttons and could
                     take back the wrong one (#1411). `grantSubject` resolves
                     the workflow subject for that second kind. The accessible
                     name leads with the visible "Remove" verb so speech-input
                     users can say the control's label (WCAG 2.5.3 label in
                     name). */
                  aria-label={`Remove ${grantSubject(g, askerNames)}'s permission: ${grantHeadline(g)} — ${
                    g.expires_at_millis <= now
                      ? "expired"
                      : `expires ${untilLabel(g.expires_at_millis, now)}`
                  } — grant ${index + 1} of ${grants.length}`}
                  disabled={busy}
                  onClick={() => {
                    mark(g.id, true);
                    void onRevoke(g.id)
                      .catch(() => {})
                      .finally(() => mark(g.id, false));
                  }}
                >
                  {busy ? (
                    <Loader2 className="size-4 animate-spin" />
                  ) : (
                    <X className="size-4" />
                  )}{" "}
                  Remove
                </Button>
              </CardContent>
            </Card>
          );
        })}
      </div>
    </section>
  );
}

/**
 * What to call whoever granted a permission.
 *
 * Never the raw actor id: that is a runtime identifier, and showing one to an
 * operator is both against the glossary rule and useless — nobody recognises a
 * uuid. When the roster cannot be read (a member, not an admin) the row says
 * "someone with admin access", which is less information but not misleading.
 */
function granterLabel(
  g: StandingGrant,
  granterNames: Map<string, string>,
): string {
  if (g.granted_by.kind !== "user") return "an automation";
  return granterNames.get(g.granted_by.id) ?? "someone with admin access";
}

/**
 * One parked approval, told in full (#372).
 *
 * The card answers the four questions an operator needs before they can decide
 * — what will happen, who is asking, what it is for, and how long it has waited
 * — and it answers the first one *concretely*, with the tool call's own
 * arguments, because that is the thing being consented to. Before this it said
 * "Shell", which asks someone to authorise an action they cannot see.
 *
 * Laid out vertically rather than as one row: the payload block needs the full
 * width, and the stacked form leaves the slot #374's per-approval scope control
 * will occupy.
 *
 * **An old host degrades to the pre-#372 card by construction.** It omits
 * `payload` and `agent` from the wire, so the payload block and the "Asked by"
 * line simply do not render when the action is named. An unlabelled native
 * action instead says that no further details were supplied (#1419), because
 * leaving its generic headline alone would not distinguish one request from
 * another.
 */
function grantDurationLabel(expiresInMillis: number): string {
  return (
    GRANT_DURATIONS.find((duration) => duration.millis === expiresInMillis)
      ?.label ?? `${Math.round(expiresInMillis / 86_400_000)} days`
  );
}

export function ApprovalCard({
  approval: a,
  now,
  askerNames,
  chatChannelByThread,
  thread,
  deciding,
  batchIndex,
  batchTotal,
  onDecide,
  extending = false,
  onExtend,
}: {
  approval: ApprovalSummary;
  now: number;
  askerNames: Map<string, string>;
  chatChannelByThread?: Readonly<Record<string, string>>;
  thread?: import("@/components/approval-card").ApprovalThreadLink | null;
  /** The verdict this card is waiting on, or `null` when it is idle (#373). */
  deciding: Verdict | null;
  /**
   * This card's 1-based position within its turn's batch — the numerator of
   * "N of M from the same turn" (#1289). `1` is the default for an approval
   * with no batch, where the line is not shown at all.
   */
  batchIndex: number;
  /**
   * How many rows this turn's batch still has waiting, including this one
   * (#842). `1` — the default for an approval with no batch — says nothing.
   */
  batchTotal: number;
  onDecide: (verdict: Verdict, scope: GrantScope) => void;
  /** Whether this card's deadline extension is in flight (#1805). */
  extending?: boolean;
  /** Push this approval's deadline out to a fresh window (#1805). Absent in
   * read-only render contexts (some tests), where the button is inert. */
  onExtend?: () => void;
}) {
  // Per-card, like the in-flight verdict and for the same reason: two cards can
  // be open at once and each carries its own decision. Defaults to `once`, so a
  // card decided without touching the control behaves exactly as it did before
  // #374 — the scope is opt-in at every level, including this one.
  const [scope, setScope] = useState<GrantScope>({ kind: "once" });
  const [declineScope, setDeclineScope] = useState<GrantScope>({
    kind: "once",
  });

  // No cross-card dimming: another card being decided is not this card's
  // business, and treating it as such is the visual half of the #373 bug.
  return (
    <Card data-approval-id={a.id}>
      <CardContent className="flex flex-col gap-3">
        {/* Issue #1406: the headline no longer carries the decide buttons.
            Approve and Decline used to sit here, level with the title and above
            everything an operator reads to decide — the payload, and the scope
            control that changes what Approve even means. On a tall card the
            scope control was ~200px below the button, or off-screen entirely,
            so the commit affordance was reachable before the evidence was seen.
            The buttons now live in a footer row below the scope control, so the
            card reads what will happen → what it will do → who asked and by
            when → decide, the order every working consent pattern uses. */}
        <ApprovalHeadline approval={a} />

        {/* Issue #596: for a workflow pre-publish gate, show the VERBATIM content
            the run is about to publish — the draft awaiting sign-off — above the
            raw payload. Additive display only; the decide/grant path below is
            untouched (epic #558). */}
        <WorkflowContentReview approval={a} />

        <ApprovalPayload approval={a} />

        <ApprovalScopeControl
          approval={a}
          askerNames={askerNames}
          scope={scope}
          onChange={setScope}
          disabled={deciding !== null}
        />
        <DeclineScopeControl
          approval={a}
          scope={declineScope}
          onChange={setDeclineScope}
          disabled={deciding !== null}
        />

        <ApprovalMeta
          approval={a}
          now={now}
          askerNames={askerNames}
          chatChannelByThread={chatChannelByThread}
          thread={thread}
          /* Honest copy for a request that spans an agent turn (#373): an
             approve is not done when the button stops spinning, it is handed
             to the agent. A decline IS terminal, so it only has to record. */
          status={
            deciding
              ? deciding === "approve"
                ? "Waiting for the teammate…"
                : "Recording…"
              : batchTotal > 1
                ? // Deliberately a count and not a link: the row is decided
                  // here, on its own, and pointing at the others would imply a
                  // batch decision this page does not offer. The conversation's
                  // card is where one Approve covers all of them (#842).
                  `${batchIndex} of ${batchTotal} from the same turn`
                : undefined
          }
        />

        {/* The decide footer (#1406) — deliberately the LAST thing in the card,
            after the scope control it depends on. Disabled on THIS card's own
            state only; a decision in flight on another card leaves these live,
            which is the whole of #373's first cause. */}
        <div
          data-testid="approval-decide"
          className="flex flex-wrap justify-end gap-2 border-t border-border pt-3"
        >
          {/* Issue #1805: the deadline is not only shown, it can be moved. Only
              offered when the host reports a deadline at all — an absent
              `expires_at_millis` means this host has no deadlines, so there is
              nothing to extend. Left of the verdicts because it is not one: it
              buys the operator time to make the actual decision. */}
          {typeof a.expires_at_millis === "number" && (
            <Button
              variant="outline"
              size="sm"
              aria-label={`Extend the deadline: ${decisionLabel(a, askerNames, now)}`}
              disabled={deciding !== null || extending}
              onClick={onExtend}
            >
              {extending ? (
                <Loader2 className="size-4 animate-spin" />
              ) : (
                <Clock className="size-4" />
              )}{" "}
              Extend
            </Button>
          )}
          <Button
            variant="outline"
            size="sm"
            /* `decisionLabel`, not `approvalAction`: two same-kind cards read
               identically from the kind alone, and button-only screen-reader
               navigation never hears the card body (#1411). */
            aria-label={`Decline: ${decisionLabel(a, askerNames, now)} — ${
              declineScope.kind === "tool"
                ? `don't ask again for this tool for ${grantDurationLabel(declineScope.expiresInMillis)}`
                : "just this once"
            }${
              // Redacted cards already carry the exact timestamp in
              // `decisionLabel`'s "composed … (…)"; appending the usual
              // `request <timestamp>` suffix here too would announce the opaque
              // epoch twice on every hidden card.
              a.contents_hidden ? "" : ` — request ${a.at_millis}`
            }${
              batchTotal > 1 ? ` — approval ${batchIndex} of ${batchTotal}` : ""
            }`}
            disabled={deciding !== null}
            /* A decline never carries a scope — there is nothing to grant,
               and the host refuses the pairing anyway. */
            onClick={() => onDecide("deny", declineScope)}
          >
            {deciding === "deny" ? (
              <Loader2 className="size-4 animate-spin" />
            ) : (
              <X className="size-4" />
            )}{" "}
            Decline
          </Button>
          <Button
            size="sm"
            aria-label={`Approve: ${decisionLabel(a, askerNames, now)} — ${
              scope.kind === "tool"
                ? `let this ${
                    a.workflow_id ? "workflow" : "teammate"
                  } use this tool for ${grantDurationLabel(scope.expiresInMillis)}`
                : "just this once"
            }${a.contents_hidden ? "" : ` — request ${a.at_millis}`}${
              batchTotal > 1 ? ` — approval ${batchIndex} of ${batchTotal}` : ""
            }`}
            disabled={deciding !== null}
            onClick={() => onDecide("approve", scope)}
          >
            {deciding === "approve" ? (
              <Loader2 className="size-4 animate-spin" />
            ) : (
              <Check className="size-4" />
            )}{" "}
            Approve
          </Button>
        </div>
      </CardContent>
    </Card>
  );
}

/** The effect kind a paused workflow gate parks as — mirrors
 * `WORKFLOW_APPROVE_KIND` in `src/runtime/workflow_resume.rs`. */
const WORKFLOW_APPROVE_KIND = "workflow.approve";
/** The payload key carrying the upstream nodes' content — mirrors
 * `PAYLOAD_CONTENT` in `src/runtime/workflow_resume.rs`. */
const PAYLOAD_CONTENT_KEY = "content";

/**
 * The "Content awaiting review" block (issue #596): for a workflow pre-publish
 * gate, the verbatim output of the nodes feeding the gate — the actual draft the
 * run is about to publish — so an operator signs off on the CONTENT, not a blind
 * approval card.
 *
 * Renders nothing for any non-workflow card, or a workflow card with no content
 * payload (an older host, or a gate with no upstream output). Read-only display
 * built from the same `run-output` parse the run inspector uses, so a draft reads
 * identically here and there.
 */
function WorkflowContentReview({ approval }: { approval: ApprovalSummary }) {
  const sections = useMemo(() => {
    if (approval.kind !== WORKFLOW_APPROVE_KIND) return [];
    if (!isRecord(approval.payload)) return [];
    const content = approval.payload[PAYLOAD_CONTENT_KEY];
    if (!isRecord(content)) return [];
    return Object.entries(content).map(([nodeId, value]) => ({
      nodeId,
      value,
      messages: parseNodeMessages(value),
    }));
  }, [approval]);

  if (sections.length === 0) return null;

  return (
    <div
      /* Blocked, not running. This arrived in the sky/running colour, which
         says "the machine is working on it" about the one state that means
         the opposite: it is parked until a person reads it. Same correction
         as `runTone`'s "awaiting approval" arm. */
      className="space-y-2 rounded-lg border border-status-blocked/30 bg-status-blocked-soft px-3 py-2"
      data-testid="workflow-content-review"
    >
      <p className="text-3xs font-medium uppercase tracking-wide text-status-blocked-text">
        Content awaiting review
      </p>
      {sections.map((section) => (
        <div key={section.nodeId} className="space-y-1">
          <p className="text-3xs uppercase tracking-wide text-muted-foreground">
            {section.nodeId}
          </p>
          {section.messages.length > 0 ? (
            section.messages.map((m, i) => (
              <div key={i} className={i > 0 ? "border-t pt-1" : undefined}>
                {m.text ? (
                  <div className="prose prose-sm max-w-none dark:prose-invert">
                    <ReactMarkdown remarkPlugins={[remarkGfm]}>
                      {m.text}
                    </ReactMarkdown>
                  </div>
                ) : (
                  <p className="whitespace-pre-wrap text-sm text-muted-foreground">
                    {m.agentRef ?? "—"}
                  </p>
                )}
              </div>
            ))
          ) : (
            // No parseable message text — show the raw value so the operator still
            // sees exactly what is about to publish rather than nothing.
            <pre className="overflow-auto whitespace-pre-wrap rounded border bg-muted/40 p-2 font-mono text-2xs leading-snug">
              {JSON.stringify(section.value, null, 2)}
            </pre>
          )}
        </div>
      ))}
    </div>
  );
}

/**
 * A filtered queue with nothing left in it (issue #883).
 *
 * Deliberately not {@link EmptyApprovals}. That one says "nothing is waiting on
 * you", which is a claim about the *whole company* — and here it would be said
 * while other cards' approvals sit one click away, unread. The two states also
 * mean opposite things to the operator who arrived from a blocked card: this
 * one says the card is free to resume, which is the answer they came for.
 */
function ClearedForTask() {
  return (
    <div className="mt-16 flex flex-col items-center gap-3 text-center">
      <div className="flex size-12 items-center justify-center rounded-2xl bg-status-done-soft text-status-done-text">
        <ShieldCheck className="size-6" />
      </div>
      <div className="space-y-1">
        <p className="font-medium">This card is clear</p>
        <p className="max-w-sm text-sm text-muted-foreground">
          Nothing it parked is still waiting on you. Other cards may still have
          approvals of their own — use{" "}
          <span className="font-medium">Show all</span> to see them.
        </p>
      </div>
    </div>
  );
}

/**
 * The queue is on the wire (issue #1229).
 *
 * A skeleton rather than the word "Loading": this page is a list, and the shape
 * of the list is the honest placeholder for it. Deliberately *not* the "All
 * clear" panel — that panel makes a claim, and nothing is known yet.
 */
function LoadingApprovals() {
  return (
    <div className="space-y-3" aria-busy="true" aria-label="Loading approvals">
      {Array.from({ length: 3 }).map((_, i) => (
        <div
          key={i}
          className="h-28 animate-pulse rounded-xl border bg-muted/40"
        />
      ))}
    </div>
  );
}

/**
 * The queue could not be read, and never has been (issue #1229).
 *
 * The page this replaces said "All clear — nothing is waiting on you" over a
 * read that failed, beside a sidebar badge that said fourteen things were.
 * On the one surface whose job is to catch what needs a person, a confident
 * false negative is the most expensive thing it can say: every parked request
 * has a deadline after which it is declined on its own.
 *
 * So this says what is actually known — nothing — and offers the only useful
 * next move. `role="alert"` because it is a correction to what the operator
 * came here to find out.
 */
function UnreadableApprovals({ onRetry }: { onRetry: () => void }) {
  return (
    <div
      role="alert"
      className="mt-16 flex flex-col items-center gap-3 text-center"
      data-testid="approvals-unreadable"
    >
      <div className="flex size-12 items-center justify-center rounded-2xl bg-destructive/10 text-destructive">
        <ShieldAlert className="size-6" />
      </div>
      <div className="space-y-1">
        <p className="font-medium">Couldn&apos;t read what&apos;s waiting</p>
        <p className="max-w-sm text-sm text-muted-foreground">
          The company host didn&apos;t answer. This is not the same as nothing
          being parked — anything waiting is still waiting, and still on its
          deadline.
        </p>
      </div>
      <Button variant="outline" size="sm" onClick={onRetry}>
        Try again
      </Button>
    </div>
  );
}

function EmptyApprovals({
  onGoToConversation,
}: {
  onGoToConversation: () => void;
}) {
  return (
    <div className="mt-16 flex flex-col items-center gap-3 text-center">
      <div className="flex size-12 items-center justify-center rounded-2xl bg-status-done-soft text-status-done-text">
        <ShieldCheck className="size-6" />
      </div>
      <div className="space-y-1">
        <p className="font-medium">All clear</p>
        <p className="max-w-sm text-sm text-muted-foreground">
          Nothing is waiting on you. Your company will park anything that needs
          a sign-off here.
        </p>
      </div>
      <Button variant="outline" size="sm" onClick={onGoToConversation}>
        Back to the conversation
      </Button>
    </div>
  );
}
