// The gated calls one turn parked, raised inside the conversation that produced
// them (#379, consolidated by #842).
//
// Chat is where an approval interrupts the work, not where an operator studies
// it. The transcript therefore carries a compact summary and one-off controls;
// the Approvals page remains the full decision surface, with the payload and
// grant-scope choice (#431). Other callers can still use this component's full
// card, because a workflow inspector is not a chat transcript.
//
// ## One card, however many calls (#842)
//
// A research turn that reaches three sites parks three approvals, and asking
// three times is the same fact told badly: it is one piece of work, and each
// interruption costs a re-dispatch cycle (#561) that can dead-end. So a batch
// renders as **one card with one Approve**, listing the hosts it covers.
//
// **All-or-nothing, deliberately.** Approve grants every call in the batch;
// Decline grants none. The operator is mid-conversation and wants one decision,
// not a form. Granularity is not missing — it lives on the Approvals page,
// which itemises the same parks one row at a time and is where an operator goes
// when they want precision or are cleaning up after the fact. Offering
// per-item control in both places would be redundant, and would double the
// state that has to stay in step between two surfaces.
//
// **What is not batched is the deciding.** One Approve resolves each item on
// its own id, so each approved call still mints its own host-scoped grant
// (#739) — three fetches produce three independently revocable standing
// permissions, exactly as they do today. Nothing about how grants are minted,
// stored or revoked changes here; only the asking is consolidated. There is no
// batch decision on the wire and no batch record on the host.
//
// A single-item batch renders exactly as this card did before #842: no list,
// no counts. The consolidation has to earn its extra furniture.
//
// The one thing it does differently from the page is how it resolves:
// **detached** (#391). The default resolve answers with the follow-up turn's
// replies, and rendering those here would put the continuation into the channel
// once from the POST body and again from its SSE echo. Detach has exactly one
// delivery path, so the duplicate-bubble race #391 deliberately left open
// outside chat POSTs cannot exist here.

import { Check, Loader2, ShieldCheck, TriangleAlert, X } from "lucide-react";
import { useMemo, useState } from "react";

import type { ApprovalSummary, GrantScope, Verdict } from "@/api/types";
import {
  ApprovalHeadline,
  ApprovalMeta,
  ApprovalPayload,
  ApprovalScopeControl,
  DeclineScopeControl,
  approvalConsequence,
  approvalIcon,
  batchConsequences,
  deadlineToneClass,
  type ApprovalThreadLink,
} from "@/components/approval-card";
import { Button, buttonVariants } from "@/components/ui/button";
import { approvedLine } from "@/lib/approval-wording";
import {
  approvalAction,
  approvalDeadline,
  money,
  payloadAge,
  payloadLeadLabel,
  payloadLeadTruncated,
} from "@/lib/language";
import { cn } from "@/lib/utils";

/** One count written with the noun it qualifies. */
function actionCount(count: number): string {
  return `${count} action${count === 1 ? "" : "s"}`;
}

/**
 * The one permanent receipt a fully settled turn leaves in the transcript
 * (#970).
 *
 * A batch's individual decisions are clicks; the turn only resumes once, when
 * the last one lands. By the time every item is in `decided`, that final
 * decision has happened, so this is the only point in chat that may honestly
 * say the agent is picking the work up. The toast and Approvals page remain
 * per-click feedback — this is deliberately the shared-channel summary.
 */
function settledReceipt(approvals: ApprovalSummary[], decided: Record<string, Verdict>): string {
  const approved = approvals.filter((a) => decided[a.id] === "approve").length;
  const declined = approvals.length - approved;

  // Preserve #561's established singular wording exactly. A one-action turn
  // earns no extra count or disclosure furniture. `undefined`, not `0`: this
  // card only knows its own item settled, not that the turn's stillAwaiting
  // count is zero — a sibling approval elsewhere in the same turn could still
  // be open, and claiming "picking it up now" here would be a guess.
  if (approvals.length === 1) {
    return approved === 1 ? approvedLine(undefined) : "Declined — recorded, and nothing will run";
  }
  if (approved === approvals.length) {
    return `Approved ${actionCount(approved)} — the teammate is picking it up now`;
  }
  if (declined === approvals.length) {
    return `Declined ${actionCount(declined)} — the teammate will not take them`;
  }
  return `Approved ${actionCount(approved)} and declined ${actionCount(declined)} — the teammate is picking it up now`;
}

/**
 * What the batch says while some of it is still undecided (#842).
 *
 * The sentence the two surfaces would otherwise drift apart on. An operator can
 * approve one row on the Approvals page while this card is on screen, and a
 * card that went on claiming three things were pending would be showing them a
 * queue that no longer exists. `decided` arrives from the shell's witnessed
 * map, which is fed by the `approval_resolved` stream frame, so this settles
 * without a reload wherever the decision was actually made.
 */
function partialLabel(settled: number, total: number): string {
  return `${settled} of ${total} decided — ${total - settled} still waiting on you`;
}

/**
 * The tint for a batch, only when every pending item has the same known
 * consequence. An unclassified item makes the aggregate meaning ambiguous, so
 * the icon remains neutral even if the other items share one consequence.
 */
function uniformConsequence(approvals: ApprovalSummary[]) {
  const consequences = batchConsequences(approvals);
  return consequences.length === 1 && approvals.every(
    (approval) => approvalConsequence(approval.group)?.label === consequences[0].label,
  )
    ? consequences[0]
    : null;
}

/**
 * What the card says when a decision did not land (#842 review).
 *
 * The failure consolidation makes worse, said out loud. Deciding three cards
 * separately, a failure belongs to the one card just clicked. Deciding one card
 * covering three, a failure on the third leaves two effects authorised and one
 * not — and a toast is both the wrong home for that (it does not say *which*)
 * and a temporary one. So the count is stated on the card, the failed rows name
 * themselves, and the buttons stay live because a retry is the way out.
 *
 * Never "nothing was recorded" unless that is true: on a batch, saying so about
 * a click that authorised two of three would be a fresh lie in place of the
 * silence it replaces.
 */
function failureLabel(failedCount: number, total: number): string {
  if (total === 1) return "Not recorded — try again";
  return failedCount === total
    ? `None of the ${total} were recorded — try again`
    : `${failedCount} of ${total} weren't recorded — try again`;
}

/**
 * One item's line in a batch: what this particular call will do.
 *
 * The **first payload line**, which is the tool's leading argument — the URL for
 * a fetch, the command for a shell call — because `PAYLOAD_KEY_ORDER` already
 * promotes the argument that is the thing being consented to. Falls back to the
 * action's own words when the host sent no payload (an old host) or withheld it
 * (#618), so a row never renders blank and never invents a value it does not
 * have.
 *
 * A small set of tools carry a second argument that changes what the first
 * does — `http_request`'s method is the difference between a read and a delete
 * on the same URL — and {@link payloadLeadLabel} puts those ahead of the lead,
 * so a row never lets the leading argument speak for an effect it does not have.
 */
function itemLabel(a: ApprovalSummary): string {
  if (a.contents_hidden) return `${approvalAction(a)} — details hidden by your role`;
  return payloadLeadLabel(a) ?? approvalAction(a);
}

/**
 * Which surface is asking, and therefore how the same decision is laid out.
 *
 * * `full` — the Approvals page and the run drawer: payload, grant scope, room
 *   to study the request.
 * * `compact` — a quiet horizontal interruption inside a chat transcript
 *   (#1330).
 * * `card` — a **board card** (#1891). Stacked, because a `w-65` column leaves
 *   roughly 220px of card content and `compact`'s single flex row does not fit
 *   in it; the buttons go full-width underneath rather than beside the label.
 *
 * A variant changes the arrangement and nothing else. Every rule about what a
 * decision *means* — the all-or-nothing batch, the per-id resolve, the
 * truncated-lead gate below — is shared by all three, which is the whole reason
 * the board card renders this component instead of its own row.
 */
export type ApprovalRowVariant = "full" | "compact" | "card";

export function ApprovalRow({
  approvals,
  now,
  askerNames,
  chatChannelByThread,
  thread,
  variant = "full",
  detailsHref = "#/approvals",
  deciding,
  decided,
  failed,
  onDecide,
}: {
  /** The gated calls this turn parked. Never empty — see `TimelineItem`. */
  approvals: ApprovalSummary[];
  now: number;
  askerNames: Map<string, string>;
  chatChannelByThread?: Readonly<Record<string, string>>;
  /** The channel this inline row is already rendered inside. */
  thread?: ApprovalThreadLink | null;
  /** How this surface lays the decision out — see {@link ApprovalRowVariant}. */
  variant?: ApprovalRowVariant;
  /**
   * Where a condensed row sends an operator who needs the full payload (#1891).
   *
   * Defaults to the whole queue, which is what chat means. A board card passes
   * `#/approvals/<taskId>` so the two places it can send somebody — "View
   * details", and the Approve that {@link needsFullReview} replaces — land on
   * that card's own rows rather than in a flat list the operator then has to
   * search for the request they were just looking at.
   */
  detailsHref?: string;
  /** The verdict an item is waiting on, keyed by approval id; empty when idle. */
  deciding: ReadonlyMap<string, Verdict>;
  /** Verdicts already witnessed — from this console or from the page. */
  decided: Record<string, Verdict>;
  /**
   * Decisions that did not land, keyed by approval id — the message to show.
   *
   * Separate from {@link decided} because a failed decision is neither: the
   * item is not settled, and it is not simply still pending either. One click
   * covering three calls can leave two authorised and one not, and an item that
   * dropped back to its pending look would read as "still working" rather than
   * "this one did not take" — the operator would believe they got all three.
   */
  failed: Record<string, string>;
  onDecide: (approval: ApprovalSummary, verdict: Verdict, scope: GrantScope) => void;
}) {
  // Per-card, exactly as on the page: two batches can be parked in one channel
  // and each carries its own decision. Defaults to `once`, so a card decided
  // without touching the control behaves as it did before #431 — the scope is
  // opt-in here too.
  const [scope, setScope] = useState<GrantScope>({ kind: "once" });
  const [declineScope, setDeclineScope] = useState<GrantScope>({ kind: "once" });

  const compact = variant === "compact";
  /**
   * Whether this surface shows a *summary* of the request rather than the
   * request.
   *
   * The distinction the truncated-lead gate below turns on, and it has to be
   * the condensed set rather than `compact` alone: a board card is the most
   * compressed surface there is, so a rule that let it one-click Approve a
   * request it could only paraphrase would open on the card exactly the hole
   * #1330 closed in chat.
   */
  const condensed = variant !== "full";

  const lead = approvals[0];
  const pending = useMemo(() => approvals.filter((a) => !decided[a.id]), [approvals, decided]);
  const settledCount = approvals.length - pending.length;
  const failedCount = pending.filter((a) => failed[a.id]).length;
  const busy = deciding.size > 0;
  // Everything decided: the card has nothing left to ask and steps back.
  const done = pending.length === 0;
  /** Whether any item in this card is waiting on `verdict` right now. */
  const awaiting = (verdict: Verdict) => [...deciding.values()].includes(verdict);
  /**
   * Whether a condensed row is asked to one-click Approve something its label
   * does not fully say.
   *
   * A body cut to fit the lead is a preview, not the payload — two POSTs to the
   * same URL whose bodies share the first 60 code units render identically even
   * when the cut-off suffix changes what the request does. The operator must
   * see the complete host-bounded payload before approving, so the inline
   * Approve is gated behind the detailed view. Scoped to `pending` because
   * that is what the button would decide; an item already settled elsewhere is
   * not part of the one-click authorisation.
   */
  const needsFullReview = condensed && pending.some((a) => payloadLeadTruncated(a));

  /**
   * The decision, applied to every item the card is still asking about.
   *
   * **Every** item, and that is what makes the card honest: the turn is blocked
   * until each call it parked has an answer (#469), so a decision that left one
   * undecided would hold the turn open while looking like it had resolved the
   * card. One click answers all of them, and the runtime continues the turn
   * once, when the last of them lands.
   *
   * `pending` and not `approvals`: an item already decided on the Approvals
   * page has been dropped by the host, and resolving it again would be a second
   * decision on an approval that no longer exists.
   *
   * A decline carries no scope — there is nothing to grant, and the host
   * refuses the pairing anyway.
   */
  const decideAll = (verdict: Verdict) => {
    for (const a of pending) {
      onDecide(a, verdict, verdict === "approve" ? scope : declineScope);
    }
  };

  // The board card's buttons are the card's own furniture, so they take the
  // Resume button's height and split the width between them rather than
  // borrowing chat's ghost treatment — which exists to keep the composer's
  // emphasis and means nothing on a Kanban column.
  const actionClass =
    variant === "compact" ? COMPACT_ACTION_CLASS : variant === "card" ? "h-7 flex-1" : undefined;
  const declineVariant = compact ? "ghost" : "outline";
  const approveVariant = compact ? "ghost" : "default";

  const actions = done ? undefined : (
    <>
      <Button
        variant={declineVariant}
        size="sm"
        className={actionClass}
        disabled={busy}
        onClick={() => decideAll("deny")}
      >
        {awaiting("deny") ? (
          <Loader2 className="size-4 animate-spin" />
        ) : (
          <X className="size-4" />
        )}{" "}
        Decline
      </Button>
      {needsFullReview ? (
        // The row's label is a preview, not the payload: a body cut to fit a
        // condensed lead is not something the operator may authorize on. Decline
        // is always safe and stays inline; Approve is replaced by a path to the
        // detailed view, where the complete host-bounded payload is on the card
        // (#1330 review).
        <a
          href={detailsHref}
          className={cn(
            buttonVariants({ variant: approveVariant, size: "sm" }),
            actionClass,
          )}
        >
          {/* The board card has no room for the page's name, and does not need
              it: `detailsHref` is that card's own rows there. What the operator
              has to be told is why Approve is not on offer — that the label
              they can see is not the whole request. */}
          {variant === "card" ? "Read it first" : "Review in Approvals"}
        </a>
      ) : (
        <Button
          variant={approveVariant}
          size="sm"
          className={actionClass}
          disabled={busy}
          onClick={() => decideAll("approve")}
        >
          {awaiting("approve") ? (
            <Loader2 className="size-4 animate-spin" />
          ) : (
            <Check className="size-4" />
          )}{" "}
          Approve
        </Button>
      )}
    </>
  );

  // Keep a completed turn visible, but compact it into one receipt instead of
  // leaving a full approval form permanently embedded in every shared channel.
  // The disclosure preserves the individual verdicts without making them the
  // transcript's primary story.
  if (done) {
    return (
      <SettledApprovalReceipt approvals={approvals} decided={decided} />
    );
  }

  // What the row says about itself while a decision is in flight, failed, or
  // partly landed. Hoisted because both condensed variants say it, and the
  // three arms are ordered: a failure outranks a partial count because it is
  // the one thing here the operator has to act on.
  const status = busy
    ? awaiting("approve")
      ? "Waiting for the teammate…"
      : "Recording…"
    : failedCount > 0
      ? failureLabel(failedCount, approvals.length)
      : settledCount > 0
        ? partialLabel(settledCount, approvals.length)
        : undefined;

  // The row speaks for what its buttons still decide, not for the whole
  // original batch: an item settled on the Approvals page or in another tab is
  // no longer something this Approve/Decline will touch, and a summary that
  // still named it would leave the operator guessing which action the
  // remaining buttons authorize (#842 review). True of both condensed
  // variants, which is why each is handed `pending`.
  if (compact) {
    return (
      <CompactApprovalRow
        approvals={pending}
        now={now}
        askerNames={askerNames}
        thread={thread}
        detailsHref={detailsHref}
        actions={actions}
        busy={busy}
        status={status}
      />
    );
  }

  if (variant === "card") {
    return (
      <BoardApprovalRow
        approvals={pending}
        now={now}
        askerNames={askerNames}
        detailsHref={detailsHref}
        actions={actions}
        busy={busy}
        status={status}
      />
    );
  }

  return (
    <div className="px-4 py-2">
      <div
        role="group"
        aria-label={approvals.length > 1 ? "Approval request for several actions" : "Approval request"}
        data-approval-id={lead.id}
        data-approval-count={approvals.length}
        className="rounded-xl border bg-card px-4 py-3 shadow-sm"
      >
        <div className="flex flex-col gap-3">
          {approvals.length > 1 ? (
            <BatchHeadline
              approvals={approvals}
              pending={pending}
              askerNames={askerNames}
              actions={actions}
            />
          ) : (
            <ApprovalHeadline approval={lead} actions={actions} />
          )}

          {approvals.length > 1 ? (
            // What the one decision covers, spelled out. Read-only: the card is
            // all-or-nothing, so a control here would offer a choice the
            // buttons below do not honour. An item settled elsewhere still says
            // so, which is how the card stops claiming three things are pending
            // when one has been decided on the page.
            <ul className="flex flex-col gap-1.5">
              {approvals.map((a) => (
                <BatchItem
                  key={a.id}
                  approval={a}
                  verdict={decided[a.id] ?? null}
                  deciding={deciding.get(a.id) ?? null}
                  failure={failed[a.id] ?? null}
                />
              ))}
            </ul>
          ) : (
            <ApprovalPayload approval={lead} />
          )}

          {/*
           * The same control the page renders, from the same module — it
           * self-gates on `broadly_grantable`, so an approval that may not be
           * granted broadly shows nothing here for exactly the reason it shows
           * nothing there. A settled card drops it: there is no decision left
           * to scope.
           *
           * Rendered against the first still-undecided item, and it means the
           * same thing for every item it covers: each approved call mints its
           * own grant, scoped to its own arguments (#739). One choice, one
           * grant per item — never one grant spanning them.
           */}
          {!done && (
            <ApprovalScopeControl
              approval={pending[0]}
              askerNames={askerNames}
              scope={scope}
              onChange={setScope}
              disabled={busy}
            />
          )}
          {!done && <DeclineScopeControl approval={pending[0]} scope={declineScope} onChange={setDeclineScope} disabled={busy} />}

          <ApprovalMeta
            approval={lead}
            now={now}
            askerNames={askerNames}
            chatChannelByThread={chatChannelByThread}
            thread={thread}
            // The same three-arm status the condensed variants show, and the
            // ordering matters here too: a failure outranks the partial count,
            // because it is the one thing the operator has to act on. It has to
            // reach a single-item card, which renders no item list to carry the
            // per-row form.
            status={status}
          />
        </div>
      </div>
    </div>
  );
}

/** The deliberately quiet pending-approval interruption used in chat (#1330). */
function CompactApprovalRow({
  approvals,
  now,
  askerNames,
  thread,
  detailsHref,
  actions,
  busy,
  status,
}: {
  /**
   * The still-undecided items the row's buttons will decide. The caller passes
   * `pending`, not the whole original batch, so a line naming them never
   * overstates what Approve/Decline covers once an item has settled elsewhere.
   */
  approvals: ApprovalSummary[];
  now: number;
  askerNames: Map<string, string>;
  /** The channel this inline row is already rendered inside (#1419). */
  thread?: ApprovalThreadLink | null;
  /** Where "View details" goes — see `ApprovalRow`'s own prop. */
  detailsHref: string;
  actions: React.ReactNode;
  busy: boolean;
  status?: React.ReactNode;
}) {
  const lead = approvals[0];
  const sameKind = approvals.every((a) => a.kind === lead.kind);
  // A mixed batch has no one glyph that is true of it, so it wears the neutral
  // one rather than the first item's, just as `BatchHeadline` does.
  const Icon = sameKind ? approvalIcon(lead.kind) : ShieldCheck;
  const consequences = batchConsequences(approvals);
  const uniform = uniformConsequence(approvals);

  return (
    <div className="px-4 py-1.5">
      <section
        aria-label={approvals.length > 1 ? "Approval request for several actions" : "Approval request"}
        data-approval-id={lead.id}
        data-approval-count={approvals.length}
        data-approval-inline="compact"
        className="flex items-center gap-2 rounded-lg px-2 py-1.5 hover:bg-muted/50"
      >
        <div
          className={cn(
            "flex size-7 shrink-0 items-center justify-center rounded-md",
            uniform?.iconClass ?? "bg-muted text-muted-foreground",
          )}
        >
          <Icon className="size-3.5" aria-hidden />
        </div>
        <div className="min-w-0 flex-1">
          <div className="flex flex-wrap items-center gap-x-2 gap-y-1">
            <CompactLabel approvals={approvals} />
            {consequences.map((c) => (
              <span
                key={c.label}
                data-approval-consequence={c.label}
                className="rounded-full bg-muted px-2 py-0.5 text-2xs font-medium text-foreground"
              >
                {c.label}
              </span>
            ))}
          </div>
          <div className="flex flex-wrap items-center gap-x-2 gap-y-1">
            <ApprovalMeta
              approval={lead}
              now={now}
              askerNames={askerNames}
              thread={thread}
              status={status}
            />
            {!busy && (
              <a
                href={detailsHref}
                className="text-xs text-muted-foreground underline-offset-4 hover:text-foreground hover:underline focus-visible:text-foreground focus-visible:underline"
              >
                View details
              </a>
            )}
          </div>
        </div>
        <div className="flex shrink-0 items-center gap-1">{actions}</div>
      </section>
    </div>
  );
}

/**
 * The decidable blocker on a board card (#1891).
 *
 * ## Why it is stacked rather than `compact`
 *
 * A board column is `w-65`, so a card has roughly 220px of content width.
 * {@link CompactApprovalRow} puts the glyph, the label, the chips, the meta
 * line and both buttons on one horizontal axis, which needs a transcript's
 * width and collapses into a stack of orphaned fragments at this one. So the
 * axis is turned: label, then chips and meta, then the buttons across the
 * bottom — the same content, arranged for the space it has.
 *
 * ## What it deliberately does not render
 *
 * {@link ApprovalMeta}'s origin pills. "Open the card" would link to the card
 * the row is drawn on, which is the same redundancy `OutputLinkRow` skips a row
 * to avoid, and the rest do not fit. The three facts that survive are the ones
 * that decide whether to act *now*: who asked, how old the payload is, and how
 * long before the deadline decides for you. All three come from the shared
 * helpers rather than being re-derived, so the board cannot end up saying
 * something the Approvals page does not.
 *
 * ## The label is not clamped
 *
 * Tempting on a card this size, and wrong for the reason {@link compactLabel}
 * exists: the line names *every* call one Approve would authorise, so cutting
 * it at three lines would let a harmless first call conceal a consequential
 * later one — "+2 more" with extra steps. A blocked card is allowed to be the
 * tall card in the column; that is what being blocked looks like.
 */
function BoardApprovalRow({
  approvals,
  now,
  askerNames,
  detailsHref,
  actions,
  busy,
  status,
}: {
  /** The still-undecided items the buttons will decide — `pending`, never the
   *  original batch. Same contract as {@link CompactApprovalRow}. */
  approvals: ApprovalSummary[];
  now: number;
  askerNames: Map<string, string>;
  /** This card's own rows on the Approvals page. */
  detailsHref: string;
  actions: React.ReactNode;
  busy: boolean;
  status?: React.ReactNode;
}) {
  const lead = approvals[0];
  const sameKind = approvals.every((a) => a.kind === lead.kind);
  // Neutral for a mixed batch, exactly as `CompactApprovalRow` and
  // `BatchHeadline` do — an envelope over a batch that also spends money would
  // be the icon quietly making a claim.
  const Icon = sameKind ? approvalIcon(lead.kind) : ShieldCheck;
  const consequences = batchConsequences(approvals);
  const uniform = uniformConsequence(approvals);
  const asker = lead.agent ? (askerNames.get(lead.agent) ?? lead.agent) : null;
  const age = payloadAge(lead, now);
  const deadline = approvalDeadline(lead.expires_at_millis ?? 0, now);

  return (
    <section
      aria-label={
        approvals.length > 1 ? "Approval request for several actions" : "Approval request"
      }
      data-approval-id={lead.id}
      data-approval-count={approvals.length}
      data-approval-inline="card"
      className="mt-2 rounded-md border border-status-blocked/30 bg-status-blocked-soft px-2 py-1.5"
    >
      <div className="flex items-start gap-1.5">
        <div
          className={cn(
            "mt-0.5 flex size-4 shrink-0 items-center justify-center rounded-sm",
            uniform?.iconClass ?? "text-status-blocked-text",
          )}
        >
          <Icon className="size-3" aria-hidden />
        </div>
        <BoardLabel approvals={approvals} />
      </div>
      {consequences.length > 0 && (
        <div className="mt-1 flex flex-wrap gap-1">
          {consequences.map((c) => (
            <span
              key={c.label}
              data-approval-consequence={c.label}
              className="rounded-full bg-muted px-1.5 py-0.5 text-3xs font-medium text-foreground"
            >
              {c.label}
            </span>
          ))}
        </div>
      )}
      <div className="mt-1 flex flex-wrap items-center gap-x-1.5 gap-y-0.5 text-2xs text-muted-foreground">
        {asker && (
          <>
            <span className="truncate">{asker}</span>
            <span aria-hidden>·</span>
          </>
        )}
        <span className={age.emphasise ? "font-medium text-foreground" : undefined}>
          {age.text}
        </span>
        {/* Only when the host reports one — never computed here. An operator
            who acted on an invented deadline would be refused, which is the
            rule `ApprovalMeta` states at length and this row obeys rather than
            restates. This is also the whole of what the board was missing: it
            counted *up* from the park and never once said the decision would be
            taken for you. */}
        {typeof lead.expires_at_millis === "number" && (
          <>
            <span aria-hidden>·</span>
            <span className={deadlineToneClass(deadline.tone)}>{deadline.text}</span>
          </>
        )}
        {status && (
          <>
            <span aria-hidden>·</span>
            <span className="text-foreground">{status}</span>
          </>
        )}
      </div>
      <div className="mt-1.5 flex items-center gap-1">{actions}</div>
      {!busy && (
        <a
          href={detailsHref}
          // Stops at the row: the card's own click handler opens the task
          // detail, and this goes somewhere else.
          onClick={(e) => e.stopPropagation()}
          className="mt-1 block text-2xs text-muted-foreground underline-offset-2 hover:text-foreground hover:underline focus-visible:text-foreground focus-visible:underline"
        >
          View details
        </a>
      )}
    </section>
  );
}

/** The board row's label — same split as {@link CompactLabel}, sized for a card. */
function BoardLabel({ approvals }: { approvals: ApprovalSummary[] }) {
  const { text, amounts } = compactLabel(approvals);
  return (
    <p className="min-w-0 flex-1 text-2xs font-medium leading-snug text-status-blocked-text">
      <span>{text}</span>
      {amounts !== "" && <span className="whitespace-nowrap">{amounts}</span>}
    </p>
  );
}

/**
 * The compact row's summary, split so nothing an Approve covers is ever
 * ellipsized away.
 *
 * A batch's line has to name what the one Approve covers, never just the first
 * call. When every call is the same action, each call's own detail is named —
 * "Fetch a web page — espn.com, bbc.com, theguardian.com" is three things, and
 * "+ 2 more" would let a harmless first call conceal a consequential second.
 * When the batch mixes actions, that phrasing would hide a payment behind a
 * fetch, so the line names every distinct action and states the count plainly,
 * the way `BatchHeadline` does — and names every call's own detail too, so a
 * URL or recipient is never hidden behind a count, and a role-hidden call
 * keeps its warning. Either way, every amount is
 * returned separately from the text so the row can keep it outside the
 * truncating region: an operator approving money must see its value whether
 * the payment is first in the batch or not, and on a narrow pane the text
 * wraps rather than ellipsizing, so a later call's detail survives to the next
 * line instead of vanishing behind "…".
 */
function compactLabel(approvals: ApprovalSummary[]): { text: string; amounts: string } {
  const lead = approvals[0];
  const action = approvalAction(lead);
  const detail = itemLabel(lead);
  // `itemLabel` already names the action for a role-hidden payload (#618);
  // restating it here would print the action twice.
  const prefix =
    lead.contents_hidden || detail === action ? detail : `${action} — ${detail}`;
  // A monetary effect shows its value beside whatever else it is doing — an
  // operator approving a payment must see its amount, whether or not the host
  // also sent a payload line to describe it.
  const amounts = approvals
    .filter((a) => a.amount_usd != null)
    .map((a) => money(a.amount_usd as number));
  const amountText = amounts.length > 0 ? ` · ${amounts.join(" · ")}` : "";

  if (approvals.length === 1) return { text: prefix, amounts: amountText };

  const rest = approvals.slice(1);

  // One action, many calls. Every call is named, not just the lead's — a
  // second command or recipient can be the consequential one, and "+ N more"
  // would hide it behind the first. A hidden call's `itemLabel` already names
  // its own action, so the comma-joined tail reads the same way.
  if (rest.every((a) => a.kind === lead.kind)) {
    const details = rest.map((a) => itemLabel(a)).join(", ");
    return { text: `${prefix}, ${details}`, amounts: amountText };
  }

  // Mixed actions: "Fetch a web page + 1 more" over a card that also sends a
  // payment would hide it, so the line says how many actions and names each
  // distinct one — never letting the lead speak for the rest — and then names
  // every call's own detail the way the same-kind path does, so a fetch's URL
  // or a payment's recipient is never hidden behind a count, and a role-hidden
  // call keeps its warning. Amounts are named for the whole batch, wherever
  // the money is.
  const kinds = [...new Set(approvals.map(approvalAction))];
  const named =
    kinds.length === 2
      ? `${kinds[0]} and ${kinds[1]}`
      : `${kinds.slice(0, -1).join(", ")}, and ${kinds[kinds.length - 1]}`;
  const details = approvals.map(itemLabel).join(", ");
  return {
    text: `${approvals.length} actions need your sign-off — ${named} — ${details}`,
    amounts: amountText,
  };
}

/** The compact row's first line: text wraps rather than ellipsizing; amounts never wrap. */
function CompactLabel({ approvals }: { approvals: ApprovalSummary[] }) {
  const { text, amounts } = compactLabel(approvals);
  return (
    <p className="flex min-w-0 items-baseline text-sm font-medium">
      <span className="min-w-0">{text}</span>
      {amounts !== "" && <span className="shrink-0">{amounts}</span>}
    </p>
  );
}

/** Quiet until an operator targets a decision; the composer keeps the emphasis. */
const COMPACT_ACTION_CLASS =
  "text-muted-foreground hover:bg-primary hover:text-primary-foreground focus-visible:bg-primary focus-visible:text-primary-foreground";

/** The compact, inspectable receipt a settled turn leaves in chat (#970). */
function SettledApprovalReceipt({
  approvals,
  decided,
}: {
  approvals: ApprovalSummary[];
  decided: Record<string, Verdict>;
}) {
  const lead = approvals[0];
  const multiple = approvals.length > 1;

  return (
    <div className="px-4 py-2">
      <section
        aria-label={multiple ? "Approval receipt for several actions" : "Approval receipt"}
        data-approval-receipt="true"
        data-approval-id={lead.id}
        data-approval-count={approvals.length}
        className="rounded-xl border bg-card px-4 py-3 text-sm shadow-sm"
      >
        <p className="font-medium">{settledReceipt(approvals, decided)}</p>
        {multiple && (
          <details className="mt-2">
            <summary className="cursor-pointer text-xs text-muted-foreground">
              Show individual decisions
            </summary>
            <ul className="mt-2 flex flex-col gap-1.5">
              {approvals.map((approval) => (
                <BatchItem
                  key={approval.id}
                  approval={approval}
                  verdict={decided[approval.id] ?? null}
                  deciding={null}
                  failure={null}
                />
              ))}
            </ul>
          </details>
        )}
      </section>
    </div>
  );
}

/**
 * The batch's headline: what the turn is asking for, and how many of them.
 *
 * Named by the shared action when every call is the same tool — the reported
 * case, three fetches from one research turn — and by a neutral count when they
 * are not, because "Fetch web pages" over a batch that also sends mail would be
 * a card that understates what approving it does.
 */
function BatchHeadline({
  approvals,
  pending,
  askerNames,
  actions,
}: {
  approvals: ApprovalSummary[];
  /** The subset one Approve would actually act on. */
  pending: ApprovalSummary[];
  askerNames: Map<string, string>;
  actions?: React.ReactNode;
}) {
  const lead = approvals[0];
  const sameKind = approvals.every((a) => a.kind === lead.kind);
  // A mixed batch has no one glyph that is true of it, so it wears the neutral
  // one rather than the first item's — an envelope over a card that also
  // deploys a website would be the icon quietly making a claim.
  const Icon = sameKind ? approvalIcon(lead.kind) : ShieldCheck;
  const asker = lead.agent ? (askerNames.get(lead.agent) ?? lead.agent) : null;
  const title = sameKind ? approvalAction(lead) : `${approvals.length} actions need your sign-off`;

  // Consolidating the asking must not consolidate away the warning (#1426).
  // Batching is the common case for exactly the calls that carry one — a
  // research turn parks several fetches, an outreach turn several sends — so a
  // batch that showed no consequence would hide it precisely where it is most
  // often earned, while the Approvals page went on showing it for the same
  // parks.
  //
  // The tint follows the same rule as the glyph above: a batch that agrees
  // with itself wears its one consequence, and a mixed batch stays neutral
  // rather than letting the first item's colour speak for a card that also
  // spends money. Either way every distinct label is listed, so nothing is
  // lost to the mixed case — and `BatchItem` repeats each line's own label, so
  // a mixed batch still says *which* call is the one that spends.
  //
  // Derived from `pending`, not from every item the card was raised with, for
  // the same reason `decideAll` iterates `pending`: the badge describes what
  // the next Approve authorises. An item settled on the Approvals page while
  // this card sat open is no longer part of that, so a batch whose only spend
  // has already been approved elsewhere must stop claiming the remaining
  // internal call spends money — that warning would be attached to a decision
  // nobody is about to make.
  const consequences = batchConsequences(pending);
  const uniform = uniformConsequence(pending);

  return (
    <div className="flex flex-wrap items-start gap-4">
      <div
        className={cn(
          "flex size-10 shrink-0 items-center justify-center rounded-lg",
          uniform?.iconClass ?? "bg-muted text-foreground",
        )}
      >
        <Icon className="size-5" />
      </div>
      {/* Same container-capped 12rem floor as `ApprovalHeadline`. */}
      <div className="min-w-[min(12rem,100%)] flex-1">
        <div className="flex flex-wrap items-center gap-x-2 gap-y-1">
          <p className="font-medium">{title}</p>
          {consequences.map((c) => (
            <span
              key={c.label}
              data-approval-consequence={c.label}
              className="rounded-full bg-muted px-2 py-0.5 text-2xs font-medium text-foreground"
            >
              {c.label}
            </span>
          ))}
        </div>
        <p className="text-xs text-muted-foreground">
          {asker ? `${asker} needs` : "This turn needs"} {approvals.length} sign-offs before it
          can carry on
        </p>
      </div>
      {actions && (
        <div data-approval-actions className="flex shrink-0 gap-2">
          {actions}
        </div>
      )}
    </div>
  );
}

/**
 * One call inside a batch: what it will do, and whether it is still waiting.
 *
 * Read-only. The card decides all-or-nothing, so a control on a line would
 * offer a choice the buttons do not honour — and the granular path already
 * exists on the Approvals page, which lists these same parks one row at a time.
 *
 * An item already decided — here, or on the page while this card was open —
 * states its verdict instead. That is the half of #842 that keeps the two
 * surfaces honest: a card cannot keep listing as pending something that has
 * already been answered somewhere else.
 */
function BatchItem({
  approval: a,
  verdict,
  deciding,
  failure,
}: {
  approval: ApprovalSummary;
  /** A verdict already witnessed for this item, or `null` while it is pending. */
  verdict: Verdict | null;
  /** The verdict this item is waiting on, or `null` when idle. */
  deciding: Verdict | null;
  /** Why this item's decision did not land, or `null` when none has failed. */
  failure: string | null;
}) {
  const label = itemLabel(a);
  // Repeated per line, not only in the headline: a mixed batch's headline says
  // the card both spends money and leaves the company, and this is what says
  // which of the three calls does which. A settled line drops it — the warning
  // is there to inform a decision that has already been made.
  const consequence = approvalConsequence(a.group);

  // A failed decision outranks the pending look, and says which item and why.
  // Silence here is the failure mode worth designing against: the operator
  // clicked once for three calls, two were authorised, and a third that merely
  // looks unstarted reads as "still working". Stated plainly, with the way back
  // — the card's own buttons are still live, so a retry is one press.
  //
  // **A settled verdict outranks it in turn**, which is why `verdict` is tested
  // first. A failure describes one *attempt*; a verdict describes the approval.
  // An item that failed here and was then resolved on the Approvals page or in
  // another tab has both, and showing "not recorded" over an approval the host
  // has already acted on would be the card contradicting the queue — the exact
  // drift this work exists to remove. The shell also clears the failure when
  // that frame arrives; this ordering is what makes the render correct
  // regardless of which state reaches it first.
  if (failure && !deciding && !verdict) {
    return (
      <li
        data-approval-item={a.id}
        data-approval-failed="true"
        className="flex flex-wrap items-center gap-x-2 gap-y-1 text-sm text-status-blocked-text"
      >
        <TriangleAlert className="size-3.5 shrink-0" aria-hidden />
        <span className="min-w-0 flex-1 truncate font-mono text-xs">{label}</span>
        {/*
         * Carried into this branch too. A failed item is still pending and the
         * card's buttons are still live, so the retry is a decision the
         * operator has yet to make — and in a mixed batch the headline says the
         * card both sends and spends without saying which row is which. Two
         * argument-classified calls sharing a kind (`composio_execute`) are
         * indistinguishable by their label alone, so dropping it here is
         * exactly where the attribution is needed most.
         */}
        {consequence && (
          <span
            data-approval-consequence={consequence.label}
            className="shrink-0 rounded-full bg-muted px-2 py-0.5 text-2xs font-medium text-foreground"
          >
            {consequence.label}
          </span>
        )}
        {/*
         * The row wraps so the badge and this line can flow beneath the label on
         * a narrow chat pane instead of both holding fixed widths and pushing
         * the label to nothing. The badge keeps `shrink-0` — a pill must not
         * squash — but a non-wrapping row left it and this text side by side,
         * and two non-shrinking pieces beside a truncating label overflow the
         * card rather than wrap.
         */}
        <span className="min-w-0 text-xs">Not recorded — {failure}</span>
      </li>
    );
  }

  return (
    // Addressable per item, because the card is no longer one approval. The
    // group keeps `data-approval-id` for the first — an existing selector still
    // finds the card the request was raised in — and this is how a caller
    // reaches any of the others.
    <li
      data-approval-item={a.id}
      // Wrapping like the failed row above: the consequence badge is
      // non-shrinking, and a non-wrapping row would let it crowd the label to
      // nothing on a narrow chat pane instead of flowing to the next line.
      className="flex flex-wrap items-center gap-x-2 gap-y-1 text-sm text-muted-foreground"
    >
      {verdict === "approve" ? (
        <Check className="size-3.5 shrink-0" />
      ) : verdict === "deny" ? (
        <X className="size-3.5 shrink-0" />
      ) : (
        <span aria-hidden className="shrink-0 text-xs">
          ·
        </span>
      )}
      <span
        className={cn(
          "min-w-0 flex-1 truncate font-mono text-xs",
          verdict ? "line-through" : "text-foreground",
        )}
      >
        {label}
      </span>
      {!verdict && consequence && (
        <span
          data-approval-consequence={consequence.label}
          className="shrink-0 rounded-full bg-muted px-2 py-0.5 text-2xs font-medium text-foreground"
        >
          {consequence.label}
        </span>
      )}
      {deciding ? (
        <Loader2 className="size-3.5 shrink-0 animate-spin" />
      ) : (
        verdict && (
          <span className="shrink-0 text-xs">
            {verdict === "approve" ? "Approved" : "Declined"}
          </span>
        )
      )}
    </li>
  );
}
