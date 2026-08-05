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
  type LucideIcon,
} from "lucide-react";

import type { OpenCompanyClient } from "@/api/client";
import type { ApprovalSummary } from "@/api/types";
import { approvalAction, money, payloadLines, timeAgo } from "@/lib/language";
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
      <span>{timeAgo(a.at_millis, now)}</span>
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
