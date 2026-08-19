// The body of an approval card, shared by the two surfaces that decide one
// (issue #379): the Approvals page and the inline card in the conversation the
// request came from.
//
// Extracted rather than duplicated because the two must say the *same thing*.
// The whole point of raising a request in the conversation is that it is the
// same request, told in full — what will happen, who is asking, what it is for,
// how long it has waited. Two copies of that would drift, and the half that
// drifted would be the one nobody was reading when it mattered.
//
// What is deliberately NOT here: the action buttons and the deciding state.
// The page's buttons resolve with the default response shape; the inline card's
// resolve detached (#391), because a body-delivered reply plus its SSE echo
// would put one continuation into the channel twice. Same content, different
// verbs — so the verbs stay with their owners.

import { useEffect, useMemo, useState } from "react";
import {
  AtSign,
  ChevronDown,
  ChevronUp,
  CreditCard,
  EyeOff,
  FileSignature,
  FileText,
  Globe,
  KeyRound,
  Mail,
  MessageSquare,
  Repeat,
  RefreshCw,
  Rocket,
  ShieldCheck,
  SquareKanban,
  Workflow,
  type LucideIcon,
} from "lucide-react";

import type { OpenCompanyClient } from "@/api/client";
import { GRANT_DURATIONS, type ApprovalSummary, type GrantScope } from "@/api/types";
import { approvalAction, money, payloadAge, payloadLines, untilLabel } from "@/lib/language";
import { cn } from "@/lib/utils";

const KIND_ICONS: Record<string, LucideIcon> = {
  "payment.send": CreditCard,
  "subscription.start": Repeat,
  "email.send": Mail,
  "dm.external": MessageSquare,
  "filing.submit": FileText,
  "contract.accept": FileSignature,
  "external.publish": Globe,
  "website.deploy": Rocket,
  "handle.register": AtSign,
  "handle.renew": RefreshCw,
  "key.rotate": KeyRound,
  "workflow.approve": Workflow,
};

/**
 * How much of a payload is shown before it is clamped. Past either bound the
 * block collapses behind a "Show everything" toggle — a queue of approvals has
 * to stay scannable, and a forty-line argument object buries the next card.
 */
const PREVIEW_LINES = 3;
const PREVIEW_VALUE_CHARS = 160;

/** The glyph for an effect kind; a shield for one this console doesn't know. */
export function approvalIcon(kind: string): LucideIcon {
  return KIND_ICONS[kind] ?? ShieldCheck;
}

/**
 * The headline row: the glyph, what will happen, and the amount when there is
 * one. Takes its actions as a slot so each surface supplies its own.
 */
export function ApprovalHeadline({
  approval: a,
  actions,
}: {
  approval: ApprovalSummary;
  actions?: React.ReactNode;
}) {
  const Icon = approvalIcon(a.kind);
  return (
    <div className="flex items-start gap-4">
      <div className="flex size-10 shrink-0 items-center justify-center rounded-lg bg-muted text-foreground">
        <Icon className="size-5" />
      </div>
      <div className="min-w-0 flex-1">
        <p className="font-medium">{approvalAction(a)}</p>
        {a.amount_usd != null && (
          <p className="text-xs font-medium text-muted-foreground">{money(a.amount_usd)}</p>
        )}
        {/*
         * #618: an absent amount normally means "this effect involves no
         * money". When it was withheld it means the opposite could be true, and
         * a member reading a hidden payment as a free action is exactly the
         * misreading the flag exists to prevent.
         */}
        {a.amount_usd == null && a.contents_hidden && (
          <p className="text-xs font-medium text-muted-foreground italic">Amount hidden</p>
        )}
      </div>
      {actions && <div className="flex shrink-0 gap-2">{actions}</div>}
    </div>
  );
}

/**
 * The footer line: who asked, which card it belongs to, how long it has waited,
 * and whatever status the surface wants to append.
 */
export function ApprovalMeta({
  approval: a,
  now,
  askerNames,
  status,
}: {
  approval: ApprovalSummary;
  now: number;
  askerNames: Map<string, string>;
  /** Trailing status text ("Waiting for the agent…", "Approved"), if any. */
  status?: React.ReactNode;
}) {
  const taskId = a.task?.link === "task" ? a.task.id : null;
  // An id the roster does not know still beats no attribution at all — the
  // operator can at least tell two askers apart.
  const asker = a.agent ? (askerNames.get(a.agent) ?? a.agent) : null;
  // #1024, computed once: the age, and whether this card should say it loudly.
  const age = payloadAge(a, now);

  return (
    <div className="flex flex-wrap items-center gap-x-2 gap-y-1 text-xs text-muted-foreground">
      {asker && (
        <>
          <span>
            Asked by <span className="font-medium text-foreground">{asker}</span>
          </span>
          <span aria-hidden>·</span>
        </>
      )}
      {taskId && (
        <>
          <a
            href={`#/tasks/${encodeURIComponent(taskId)}`}
            className="flex w-fit items-center gap-1 rounded-full bg-accent px-2 py-0.5 font-medium text-accent-foreground transition-opacity hover:opacity-80"
          >
            <SquareKanban className="size-3 shrink-0" />
            Open the card
          </a>
          <span aria-hidden>·</span>
        </>
      )}
      {/*
       * #1024. The same integer means two different things depending on where it
       * sits. In this footer, between "Asked by Maya" and "Open the card", a bare
       * "5d ago" reads as QUEUE LATENCY — how long the operator's backlog has held
       * this — a fact about the queue. What decides an outbound send is that the
       * PAYLOAD is five days old, a fact about the content. A digest built from
       * 13 Aug mailed as "Weekly Digest — 18 Aug" the moment a backlog was cleared,
       * and the report says why nobody caught it: "from the operator's side it
       * looked like a routine send." The signal was not missing — it was
       * unlabelled, and dressed as routing metadata.
       *
       * Wording and emphasis both come from `payloadAge`, so they are testable as
       * a string rather than only as rendered output.
       */}
      <span className={age.emphasise ? "font-medium text-foreground" : undefined}>
        {age.text}
      </span>
      {/* The deadline (#971), beside how old the payload is — the two halves of
          "is this still worth deciding?".

          Rendered only when the host reports one. An absent
          `expires_at_millis` means the host does not have deadlines, NOT that
          this card has none, so the console shows nothing rather than
          computing a deadline nothing would enforce: an operator who acted on
          an invented "in 3h" would be refused. */}
      {typeof a.expires_at_millis === "number" && (
        <>
          <span aria-hidden>·</span>
          <span>declined {untilLabel(a.expires_at_millis, now)}</span>
        </>
      )}
      {status && (
        <>
          <span aria-hidden>·</span>
          <span className="text-foreground">{status}</span>
        </>
      )}
    </div>
  );
}

/**
 * The tool call's own arguments, verbatim (#372) — `null` when the effect
 * carries none, so a caller can skip the block entirely.
 *
 * Monospace and wrapping rather than truncating: a shell command cut off
 * mid-flag is exactly as un-decidable as no command at all, and `break-all` is
 * what keeps a long unbroken path or URL inside the card. Everything here was
 * redacted and bounded by the host, so `[redacted]` is a value the console
 * renders — never one it computes.
 */
export function ApprovalPayload({ approval }: { approval: ApprovalSummary }) {
  const lines = useMemo(() => payloadLines(approval), [approval]);
  const [expanded, setExpanded] = useState(false);

  // Withheld by role (#618) — say so. Returning `null` here would be the one
  // wrong answer: it is what an approval with no arguments renders as, so a
  // member would read a hidden payment as an ordinary empty card. The point of
  // the flag is that "nothing to show" and "not shown to you" must not look
  // alike.
  if (approval.contents_hidden) {
    return (
      <div className="flex items-center gap-2 rounded-lg border border-dashed bg-muted/40 px-3 py-2 text-xs text-muted-foreground">
        <EyeOff className="size-3.5 shrink-0" />
        <span>
          Details hidden by your role. An admin can see what this approval will do and decide it.
        </span>
      </div>
    );
  }

  if (lines.length === 0) return null;

  const clampable =
    lines.length > PREVIEW_LINES || lines.some((l) => l.value.length > PREVIEW_VALUE_CHARS);
  const shown = expanded || !clampable ? lines : lines.slice(0, PREVIEW_LINES);

  return (
    <div className="rounded-lg border bg-muted/40 px-3 py-2">
      <div
        className={cn(
          "space-y-1 font-mono text-xs break-all whitespace-pre-wrap",
          clampable && !expanded && "max-h-24 overflow-hidden",
        )}
      >
        {shown.map((line) => (
          <div key={line.label}>
            <span className="text-muted-foreground">{line.label}: </span>
            <span className="text-foreground">{line.value}</span>
          </div>
        ))}
      </div>
      {clampable && (
        <button
          type="button"
          onClick={() => setExpanded((v) => !v)}
          className="mt-1.5 flex items-center gap-1 text-xs text-muted-foreground transition-colors hover:text-foreground"
        >
          {expanded ? <ChevronUp className="size-3" /> : <ChevronDown className="size-3" />}
          {expanded ? "Show less" : "Show everything"}
        </button>
      )}
    </div>
  );
}

/**
 * The scope control: what this approve buys (#374).
 *
 * Rendered **only** when the host marked the card `broadly_grantable`, so the
 * operator is never offered a choice the host would refuse. That is UX, not the
 * boundary — the host re-checks and answers 400 — but offering an option that
 * cannot work is its own kind of lie.
 *
 * Two options, and the default needs no interaction at all: doing nothing
 * approves once, exactly as before this existed. Picking the broader option
 * forces a duration, because there is no unbounded form to fall back to; the
 * radio and the duration are one control rather than two so an operator cannot
 * arrive at "for a period, unspecified".
 *
 * Lives here rather than in either view because both surfaces that decide an
 * approval must say the same thing about what a decision means. Two copies of
 * this wording would drift, and the half that drifted would be the one somebody
 * was reading when it mattered.
 */
export function ApprovalScopeControl({
  approval: a,
  askerNames,
  scope,
  onChange,
  disabled,
}: {
  approval: ApprovalSummary;
  /** Roster names, so the sentence says who — the same map the meta line uses. */
  askerNames: Map<string, string>;
  scope: GrantScope;
  onChange: (scope: GrantScope) => void;
  disabled?: boolean;
}) {
  const name = `scope-${a.id}`;
  if (!a.broadly_grantable) return null;

  return (
    <fieldset
      disabled={disabled}
      className="rounded-lg border bg-muted/30 px-3 py-2 text-sm disabled:opacity-60"
    >
      <legend className="px-1 text-xs text-muted-foreground">If you approve</legend>
      <div className="flex flex-col gap-1.5">
        <label className="flex items-center gap-2">
          <input
            type="radio"
            name={name}
            checked={scope.kind === "once"}
            onChange={() => onChange({ kind: "once" })}
            className="size-3.5 accent-primary"
          />
          <span>Just this once</span>
        </label>
        <label className="flex flex-wrap items-center gap-2">
          <input
            type="radio"
            name={name}
            checked={scope.kind === "tool"}
            // Picking the broader scope commits to a duration immediately —
            // the first option, not an empty one — so there is no state in
            // which "for a period" is selected with no period.
            onChange={() =>
              onChange({ kind: "tool", expiresInMillis: GRANT_DURATIONS[0].millis })
            }
            className="size-3.5 accent-primary"
          />
          <span>Let {askerLabel(a, askerNames)} use this tool for</span>
          <select
            value={scope.kind === "tool" ? scope.expiresInMillis : GRANT_DURATIONS[0].millis}
            disabled={scope.kind !== "tool"}
            onChange={(e) =>
              onChange({ kind: "tool", expiresInMillis: Number(e.target.value) })
            }
            aria-label="How long this permission lasts"
            className="rounded-md border bg-background px-1.5 py-0.5 text-xs disabled:opacity-50"
          >
            {GRANT_DURATIONS.map((d) => (
              <option key={d.millis} value={d.millis}>
                {d.label}
              </option>
            ))}
          </select>
        </label>
      </div>
      {scope.kind === "tool" && (
        <p className="mt-1.5 px-1 text-xs text-muted-foreground">
          It won't ask again for this tool until then — with any arguments. You can take it
          back from Standing permissions at any time.
        </p>
      )}
    </fieldset>
  );
}

/**
 * Who the broader scope would be granted to, by name.
 *
 * Falls back to the raw agent id, then to "this teammate". Naming the wrong
 * teammate would be worse than naming none, so this only ever narrows from what
 * the host actually said — it never guesses.
 */
function askerLabel(a: ApprovalSummary, askerNames: Map<string, string>): string {
  if (!a.agent) return "this teammate";
  return askerNames.get(a.agent) ?? a.agent;
}

/**
 * Agent id → display name, for the "Asked by" line.
 *
 * One roster read per company, not one per card: the ids on the queue are
 * roster ids, and the roster is small and stable. A host without the roster
 * route 404s, which is caught here — the card then shows the raw id rather than
 * dropping the attribution, because "which teammate asked" stays useful even
 * when we cannot pretty-print it.
 */
export function useAskerNames(
  client: OpenCompanyClient,
  company: string | null,
  approvals: ApprovalSummary[],
): Map<string, string> {
  const [names, setNames] = useState<Map<string, string>>(new Map());
  // Keyed on the set of asker ids rather than on `approvals` itself: the feed
  // hands us a fresh array on every poll, and depending on the array would
  // refetch the roster every few seconds for a roster that rarely changes.
  const askerKey = useMemo(
    () =>
      Array.from(new Set(approvals.map((a) => a.agent).filter((id): id is string => !!id)))
        .sort()
        .join(","),
    [approvals],
  );

  useEffect(() => {
    if (!askerKey) return;
    let live = true;
    void (async () => {
      const roster = await client.listTeam(company).catch(() => []);
      if (!live) return;
      setNames(new Map(roster.map((m) => [m.id, m.name?.trim() || m.role])));
    })();
    return () => {
      live = false;
    };
  }, [client, company, askerKey]);

  return names;
}
