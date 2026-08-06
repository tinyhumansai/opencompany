import { useCallback, useEffect, useState } from "react";
import { Check, Loader2, ShieldCheck, X } from "lucide-react";
import { toast } from "sonner";

import type { OpenCompanyClient } from "@/api/client";
import {
  ApiError,
  type ApprovalSummary,
  type GrantScope,
  type StandingGrant,
  type Verdict,
} from "@/api/types";
import {
  ApprovalHeadline,
  ApprovalMeta,
  ApprovalPayload,
  ApprovalScopeControl,
  useAskerNames,
} from "@/components/approval-card";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import type { CompanyFeed } from "@/hooks/use-company";
import { approvalSummary, timeAgo, toolAction } from "@/lib/language";

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
  onResolved: (systemLine: string) => void;
  onGoToConversation: () => void;
}

/** The approvals inbox: the few things the company parked for the operator. */
export function ApprovalsView({ client, company, feed, onResolved, onGoToConversation }: Props) {
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
  const [inFlight, setInFlight] = useState<ReadonlyMap<string, Verdict>>(() => new Map());
  const { approvals, now } = feed;
  const askerNames = useAskerNames(client, company, approvals);
  const { grants, granterNames, refreshGrants } = useStandingGrants(client, company);

  const markInFlight = (id: string, verdict: Verdict | null) =>
    setInFlight((prev) => {
      const next = new Map(prev);
      if (verdict) next.set(id, verdict);
      else next.delete(id);
      return next;
    });

  async function decide(a: ApprovalSummary, verdict: Verdict, scope: GrantScope) {
    // Per-row guard: only a double-press on THIS card is ignored. The global
    // early return that used to live here made every other card inert too.
    if (inFlight.has(a.id)) return;
    markInFlight(a.id, verdict);
    const startedAt = Date.now();
    try {
      await client.resolveApproval(a.id, verdict, undefined, company, { scope });
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
      // …and only say "the agent" when there IS one (#395). A card with no
      // `agent` is one the runtime performs itself — a paused workflow gate, a
      // cold-recipient report — and naming an agent there is the same shape of
      // small lie the wording above exists to remove. The work is still in
      // flight either way, so both halves say so; only the actor changes.
      const line =
        verdict !== "approve"
          ? `Declined: ${approvalSummary(a)}`
          : scope.kind === "tool"
            ? `Approved — ${toolAction(a.kind).toLowerCase()} won't ask again until this permission expires. Take it back under Standing permissions.`
            : a.agent
              ? `Approved — the agent is completing the action: ${approvalSummary(a)}`
              : `Approved — carrying it out now: ${approvalSummary(a)}`;
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
            ? "Approved — the host didn't answer in time, but your decision was recorded. The agent may still be working; no need to approve again."
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
        const msg = err instanceof ApiError ? err.message : "something went wrong";
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

  return (
    <div className="flex-1 overflow-y-auto">
      <div className="mx-auto w-full max-w-3xl px-4 py-6">
        {approvals.length === 0 ? (
          <EmptyApprovals onGoToConversation={onGoToConversation} />
        ) : (
          <>
            <div className="mb-4 flex items-baseline justify-between">
              <h2 className="text-sm font-medium text-muted-foreground">
                {approvals.length === 1
                  ? "1 thing needs your approval"
                  : `${approvals.length} things need your approval`}
              </h2>
            </div>
            <div className="flex flex-col gap-3">
              {approvals.map((a) => (
                <ApprovalCard
                  key={a.id}
                  approval={a}
                  now={now}
                  askerNames={askerNames}
                  deciding={inFlight.get(a.id) ?? null}
                  onDecide={(verdict, scope) => void decide(a, verdict, scope)}
                />
              ))}
            </div>
          </>
        )}

        {/* Below the queue, and shown even when the queue is empty — a standing
            permission with nothing currently parked is exactly the state an
            operator most needs to be able to find and take back. */}
        <StandingPermissions
          grants={grants}
          now={now}
          askerNames={askerNames}
          granterNames={granterNames}
          onRevoke={async (id) => {
            try {
              await client.revokeGrant(id, company);
              toast.success("Permission revoked — this tool will ask again from its next call.");
            } catch (err) {
              // A 404 means it was already gone (revoked elsewhere, or expired).
              // The operator's intent is satisfied either way, so this is not an
              // error to them — only a stale list, which the refresh below fixes.
              if (err instanceof ApiError && err.status === 404) {
                toast.info("That permission was already gone.");
              } else {
                const msg = err instanceof ApiError ? err.message : "something went wrong";
                toast.error(`Couldn't revoke it — ${msg}`);
                throw err;
              }
            } finally {
              void refreshGrants();
            }
          }}
        />
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
function useStandingGrants(client: OpenCompanyClient, company: string | null) {
  const [grants, setGrants] = useState<StandingGrant[]>([]);
  // Actor id → what to call them. Without this the row reads "granted by
  // 019fd3d1bddf-000000000005", which is a runtime identifier in front of an
  // operator — the thing the glossary rule forbids, and useless besides.
  const [granterNames, setGranterNames] = useState<Map<string, string>>(new Map());

  const refreshGrants = useCallback(async () => {
    const next = await client.listGrants(company).catch(() => [] as StandingGrant[]);
    setGrants(next);
  }, [client, company]);

  useEffect(() => {
    let live = true;
    void (async () => {
      const next = await client.listGrants(company).catch(() => [] as StandingGrant[]);
      if (live) setGrants(next);
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
      setGranterNames(names);
    })();

    // Slow on purpose. Mint and revoke from another browser are only
    // poll-visible in v1 (there is no event for them), and a permission list is
    // not something an operator watches change — a minute is well inside the
    // shortest duration on offer.
    const timer = setInterval(() => void refreshGrants(), 60_000);
    return () => {
      live = false;
      clearInterval(timer);
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
function StandingPermissions({
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
  const [revoking, setRevoking] = useState<ReadonlySet<string>>(() => new Set());
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
      <h2 className="mb-1 text-sm font-medium text-muted-foreground">Standing permissions</h2>
      <p className="mb-3 text-xs text-muted-foreground">
        Tools you've let a teammate use without asking each time. Each one expires on its own;
        you can end it sooner.
      </p>
      <div className="flex flex-col gap-2">
        {grants.map((g) => {
          const busy = revoking.has(g.id);
          const expired = g.expires_at_millis <= now;
          return (
            <Card key={g.id}>
              <CardContent className="flex flex-wrap items-center gap-3 py-3">
                <div className="min-w-0 flex-1">
                  {/* Phrased, never the raw identifier — the glossary rule. */}
                  <p className="truncate text-sm font-medium">{grantHeadline(g)}</p>
                  <p className="text-xs text-muted-foreground">
                    {askerNames.get(g.agent) ?? g.agent} ·{" "}
                    {expired ? "expired" : `expires ${untilLabel(g.expires_at_millis, now)}`} ·
                    granted {timeAgo(g.at_millis, now)} by {granterLabel(g, granterNames)}
                  </p>
                </div>
                <Button
                  variant="outline"
                  size="sm"
                  disabled={busy}
                  onClick={() => {
                    mark(g.id, true);
                    void onRevoke(g.id)
                      .catch(() => {})
                      .finally(() => mark(g.id, false));
                  }}
                >
                  {busy ? <Loader2 className="size-4 animate-spin" /> : <X className="size-4" />}{" "}
                  Revoke
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
 * What one standing permission actually covers, in one line (#457).
 *
 * `toolAction` alone answers "which tool", which is the whole sentence for
 * `file_write` and only half of it for `composio_execute`: that name carries
 * every action of every connected provider, so a row reading "Act in one of its
 * connected accounts" leaves an operator unable to tell a permission over their
 * code host from one over their mailbox — and a permission you cannot read is
 * one you cannot decide to revoke. When the host names a scope, the row names
 * the provider.
 *
 * Composed as a suffix rather than a second phrasing table: the tool's own words
 * stay in `lib/language`, so there is nothing here to drift out of step with it.
 */
function grantHeadline(g: StandingGrant): string {
  const action = toolAction(g.tool);
  return g.scope ? `${action} — ${providerLabel(g.scope)} only` : action;
}

/**
 * A toolkit identifier as a person would write it: `microsoft_teams` →
 * "Microsoft Teams".
 *
 * Mechanical on purpose. A lookup table of pretty provider names would render
 * the ~30 toolkits it knew and the raw slug for everything else, and the ones it
 * did not know would be exactly the ones added most recently.
 */
function providerLabel(scope: string): string {
  return scope
    .split("_")
    .filter(Boolean)
    .map((word) => word.charAt(0).toUpperCase() + word.slice(1))
    .join(" ");
}

/**
 * What to call whoever granted a permission.
 *
 * Never the raw actor id: that is a runtime identifier, and showing one to an
 * operator is both against the glossary rule and useless — nobody recognises a
 * uuid. When the roster cannot be read (a member, not an admin) the row says
 * "someone with admin access", which is less information but not misleading.
 */
function granterLabel(g: StandingGrant, granterNames: Map<string, string>): string {
  if (g.granted_by.kind !== "user") return "an automation";
  return granterNames.get(g.granted_by.id) ?? "someone with admin access";
}

/** "in 42m" / "in 6h" / "in 3d" — how long a permission has left. */
function untilLabel(atMillis: number, now: number): string {
  const mins = Math.max(0, Math.round((atMillis - now) / 60_000));
  if (mins < 60) return `in ${mins}m`;
  const hours = Math.round(mins / 60);
  if (hours < 24) return `in ${hours}h`;
  return `in ${Math.round(hours / 24)}d`;
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
 * line simply do not render and what is left is the headline, the amount and
 * the relative time — exactly what shipped before.
 */
function ApprovalCard({
  approval: a,
  now,
  askerNames,
  deciding,
  onDecide,
}: {
  approval: ApprovalSummary;
  now: number;
  askerNames: Map<string, string>;
  /** The verdict this card is waiting on, or `null` when it is idle (#373). */
  deciding: Verdict | null;
  onDecide: (verdict: Verdict, scope: GrantScope) => void;
}) {
  // Per-card, like the in-flight verdict and for the same reason: two cards can
  // be open at once and each carries its own decision. Defaults to `once`, so a
  // card decided without touching the control behaves exactly as it did before
  // #374 — the scope is opt-in at every level, including this one.
  const [scope, setScope] = useState<GrantScope>({ kind: "once" });

  // No cross-card dimming: another card being decided is not this card's
  // business, and treating it as such is the visual half of the #373 bug.
  return (
    <Card>
      <CardContent className="flex flex-col gap-3 py-4">
        <ApprovalHeadline
          approval={a}
          /* Disabled on THIS card's own state only — a decision in flight on
             another card leaves these live. That is the whole of #373's
             first cause. */
          actions={
            <>
              <Button
                variant="outline"
                size="sm"
                disabled={deciding !== null}
                /* A decline never carries a scope — there is nothing to grant,
                   and the host refuses the pairing anyway. */
                onClick={() => onDecide("deny", { kind: "once" })}
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
            </>
          }
        />

        <ApprovalPayload approval={a} />

        <ApprovalScopeControl
          approval={a}
          askerNames={askerNames}
          scope={scope}
          onChange={setScope}
          disabled={deciding !== null}
        />

        <ApprovalMeta
          approval={a}
          now={now}
          askerNames={askerNames}
          /* Honest copy for a request that spans an agent turn (#373): an
             approve is not done when the button stops spinning, it is handed
             to the agent. A decline IS terminal, so it only has to record. */
          status={
            deciding ? (deciding === "approve" ? "Waiting for the agent…" : "Recording…") : undefined
          }
        />
      </CardContent>
    </Card>
  );
}

function EmptyApprovals({ onGoToConversation }: { onGoToConversation: () => void }) {
  return (
    <div className="mt-16 flex flex-col items-center gap-3 text-center">
      <div className="flex size-12 items-center justify-center rounded-2xl bg-emerald-500/10 text-emerald-600 dark:text-emerald-400">
        <ShieldCheck className="size-6" />
      </div>
      <div className="space-y-1">
        <p className="font-medium">All clear</p>
        <p className="max-w-sm text-sm text-muted-foreground">
          Nothing is waiting on you. Your company will park anything that needs a sign-off here.
        </p>
      </div>
      <Button variant="outline" size="sm" onClick={onGoToConversation}>
        Back to the conversation
      </Button>
    </div>
  );
}
