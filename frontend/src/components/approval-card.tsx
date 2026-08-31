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

import { useEffect, useMemo, useState, useRef } from "react";
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
import {
  GRANT_DURATIONS,
  type ApprovalSummary,
  type GrantScope,
} from "@/api/types";
import { MAIN_THREAD_ID } from "@/lib/chat";
import { GENERAL_CHANNEL, type Desk } from "@/lib/desks";
import {
  approvalAction,
  approvalDeadline,
  type DeadlineTone,
  money,
  payloadAge,
  payloadLines,
} from "@/lib/language";
import { fromDto, type TeamMember } from "@/lib/team";
import { cn } from "@/lib/utils";
import { workflowHref } from "@/lib/task-output";
import {
  channelForThread,
  channelIdForThread,
  deskFromDto,
  dmChannelId,
  memberForThread,
} from "@/views/chat/model";

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
 * The host's consequence group in the operator's vocabulary (#1426).
 *
 * `group` is derived from the tool call and its arguments by the host, which
 * is the only layer that can know the actual consequence. `other` is the
 * internal catch-all, while an absent group means an older host: both stay
 * deliberately unmarked so the badge and tint retain their signal.
 *
 * The tints come from the identity palette (`--tone-1` … `--tone-5`), not from
 * `--status-*`. A consequence group is a category, which is what that palette
 * exists for — `docs/brand/README.md` ("Identity is not status") reserves the
 * five status hues for run state and says not to reuse one for anything that
 * is not that status. These badges did: a pending hire approval was painted
 * the green that means "finished cleanly", and spend the red that means
 * "failed". The identity palette deliberately holds no amber, green or red,
 * so a queue of approvals can no longer read as a queue of run outcomes.
 *
 * Six groups over five tones, so `spend` and `hire` share tone 4 — the two
 * that most often move the same money. Sharing is safe here and precedented
 * (`lib/team.ts`, `lib/skills.ts` both fold more names onto these five): the
 * badge always carries its own icon and label, so colour is never the only
 * carrier of the distinction, which is the rule the brand doc actually sets.
 */
const APPROVAL_CONSEQUENCES = {
  spend: { label: "Spends money", iconClass: "bg-tone-4/15 text-tone-4-text" },
  send: { label: "Leaves the company", iconClass: "bg-tone-2/15 text-tone-2-text" },
  sign: { label: "Makes a commitment", iconClass: "bg-tone-1/15 text-tone-1-text" },
  // Not "Goes public". The group spans genuinely external publishing —
  // `repo_publish`'s push to the real remote, `hosting_launch_site`,
  // `hosting_add_domain` — *and* `publish_artifact`, which writes only into the
  // company's own workspace and artifact chain and sends nothing anywhere. A
  // card reading "Goes public" over that hand-off is the misleading label
  // `language.ts` already refuses to print for the same tool. "Publishes work"
  // is true under either reading: it is the step that makes finished work
  // visible past the agent's sandbox, whoever is on the other side of it.
  publish: { label: "Publishes work", iconClass: "bg-tone-3/15 text-tone-3-text" },
  // `hire` and `identity` are separate rows in the taxonomy
  // (`docs/spec/company-brain/approvals.md`) and had been sharing one label,
  // which hid exactly the distinction this change exists to draw. `hire` is an
  // outbound engagement with another company or the firing of a vendor;
  // `identity` is handle registration and renewal, key rotation, delegated
  // signer mint/expand and `composio_authorize` — the company's own name and
  // credentials, not who it does business with.
  hire: { label: "Engages or drops a counterparty", iconClass: "bg-tone-4/15 text-tone-4-text" },
  identity: { label: "Changes its identity or keys", iconClass: "bg-tone-5/15 text-tone-5-text" },
} as const;

type ApprovalConsequence = (typeof APPROVAL_CONSEQUENCES)[keyof typeof APPROVAL_CONSEQUENCES];

/** The marked consequence, or nothing for internal and old-host approvals. */
export function approvalConsequence(group: ApprovalSummary["group"]): ApprovalConsequence | null {
  if (group == null || group === "other") return null;
  return APPROVAL_CONSEQUENCES[group];
}

/**
 * Every distinct consequence a batch of approvals carries, in the order the
 * batch first raises each one.
 *
 * A turn that parks several calls renders as one card with one Approve, so the
 * warning has to survive the consolidation: a batch of three outbound sends
 * still leaves the company, and a mixed batch spends money *and* leaves it.
 * Returning the distinct set rather than a single verdict is what lets the
 * headline stay honest either way — one badge when the batch agrees with
 * itself, one per consequence when it does not.
 *
 * Deduplicated by label, not by group: `hire` and `identity` are separate
 * groups, while `spend` and `hire` share a tint, so the label is the thing an
 * operator actually reads twice.
 */
export function batchConsequences(approvals: ApprovalSummary[]): ApprovalConsequence[] {
  const seen = new Set<string>();
  const out: ApprovalConsequence[] = [];
  for (const a of approvals) {
    const consequence = approvalConsequence(a.group);
    if (!consequence || seen.has(consequence.label)) continue;
    seen.add(consequence.label);
    out.push(consequence);
  }
  return out;
}

/**
 * How much of a payload is shown before it is clamped. Past either bound the
 * block collapses behind a "Show everything" toggle — at a line boundary, so
 * a queue of approvals stays scannable without clipping a line's glyphs.
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
  const consequence = approvalConsequence(a.group);
  return (
    <div className="flex flex-wrap items-start gap-4">
      <div
        className={cn(
          "flex size-10 shrink-0 items-center justify-center rounded-lg",
          consequence?.iconClass ?? "bg-muted text-foreground",
        )}
      >
        <Icon className="size-5" />
      </div>
      {/* 12rem floor, capped at the card's own width: a chat column can be
          narrower than the icon plus a 12rem title, and a hard floor would
          overflow the card there instead of wrapping. */}
      <div className="min-w-[min(12rem,100%)] flex-1">
        <div className="flex flex-wrap items-center gap-x-2 gap-y-1">
          <p className="font-medium">{approvalAction(a)}</p>
          {consequence && (
            <span className="rounded-full bg-muted px-2 py-0.5 text-2xs font-medium text-foreground">
              {consequence.label}
            </span>
          )}
        </div>
        {a.amount_usd != null && (
          <p className="text-xs font-medium text-muted-foreground">
            {money(a.amount_usd)}
          </p>
        )}
        {/*
         * #618: an absent amount normally means "this effect involves no
         * money". When it was withheld it means the opposite could be true, and
         * a member reading a hidden payment as a free action is exactly the
         * misreading the flag exists to prevent.
         */}
        {a.amount_usd == null && a.contents_hidden && (
          <p className="text-xs font-medium text-muted-foreground italic">
            Amount hidden
          </p>
        )}
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
 * The footer line: who asked, which card it belongs to, how long it has waited,
 * and whatever status the surface wants to append.
 */
export function ApprovalMeta({
  approval: a,
  now,
  askerNames,
  chatChannelByThread,
  thread,
  status,
}: {
  approval: ApprovalSummary;
  now: number;
  askerNames: Map<string, string>;
  /** Host thread id → console channel id, resolved with `channelIdForThread`. */
  chatChannelByThread?: Readonly<Record<string, string>>;
  /** The chat channel that raised this request, when the host named one. */
  thread?: ApprovalThreadLink | null;
  /** Trailing status text ("Waiting for the teammate…", "Approved"), if any. */
  status?: React.ReactNode;
}) {
  const taskId = a.task?.link === "task" ? a.task.id : null;
  // Resolved through `channelForThread`, not a bare `map[key]` index: the
  // host compares General spellings case-insensitively and echoes back
  // whichever one it was addressed with, so an approval raised in `#general`
  // can carry a thread id the map's own literal keys miss (issue #1781
  // review, Codex P2). A bare index breaks the "Asked in" link for exactly
  // that case.
  const conversationChannelId =
    a.thread && chatChannelByThread ? channelForThread(chatChannelByThread, a.thread) : null;
  const workflowId = workflowIdForApproval(a);
  const workflowRunHref =
    workflowId && a.workflow_run_id ? workflowHref(workflowId, a.workflow_run_id) : null;
  // The "Asked in" link above renders straight from `thread`, which the host
  // resolved against the desk/roster on its own — independent of the shell's
  // separate chat-topology hydration. When that hydration is still in flight
  // (or failed), `chatChannelByThread` is empty but the origin is still
  // visibly available, so counting `thread` keeps the footer from saying
  // "Origin unavailable" underneath a live link.
  const hasOriginLink =
    thread != null ||
    taskId !== null ||
    conversationChannelId !== null ||
    workflowRunHref !== null;
  // An id the roster does not know still beats no attribution at all — the
  // operator can at least tell two askers apart.
  const asker = a.agent ? (askerNames.get(a.agent) ?? a.agent) : null;
  // #1024, computed once: the age, and whether this card should say it loudly.
  const age = payloadAge(a, now);
  // #1403, likewise: the deadline's words and how loudly to say them. Computed
  // unconditionally and read only inside the guard below, so a host that
  // reports no deadline still renders nothing.
  const deadline = approvalDeadline(a.expires_at_millis ?? 0, now);

  return (
    <div className="flex flex-wrap items-center gap-x-2 gap-y-1 text-xs text-muted-foreground">
      {asker && (
        <>
          <span>
            Asked by{" "}
            <span className="font-medium text-foreground">{asker}</span>
          </span>
          <span aria-hidden>·</span>
        </>
      )}
      {thread && (
        <>
          <span>
            Asked in{" "}
            <a
              // Written raw, not `encodeURIComponent`-ed: a DM's channel id is
              // `dm:<agent-id>`, and the hash router splits `#/chat/…` on "/"
              // without decoding, so an encoded `:` would look up a channel
              // that does not exist. Every channel id is a slug or `dm:<uuid>`,
              // which the hash already allows unescaped.
              href={`#/chat/${thread.channelId}`}
              className="font-medium text-foreground underline-offset-2 hover:underline"
            >
              {thread.label}
            </a>
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
      {conversationChannelId && (
        <>
          <a
            href={`#/chat/${encodeURIComponent(conversationChannelId)}`}
            className="flex w-fit items-center gap-1 rounded-full bg-accent px-2 py-0.5 font-medium text-accent-foreground transition-opacity hover:opacity-80"
          >
            <MessageSquare className="size-3 shrink-0" />
            Open the conversation
          </a>
          <span aria-hidden>·</span>
        </>
      )}
      {workflowRunHref && (
        <>
          <a
            href={workflowRunHref}
            className="flex w-fit items-center gap-1 rounded-full bg-accent px-2 py-0.5 font-medium text-accent-foreground transition-opacity hover:opacity-80"
          >
            <Workflow className="size-3 shrink-0" />
            Open the run
          </a>
          <span aria-hidden>·</span>
        </>
      )}
      {!hasOriginLink && (
        <>
          <span>Origin unavailable</span>
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
      <span
        className={age.emphasise ? "font-medium text-foreground" : undefined}
      >
        {age.text}
      </span>
      {/* The deadline (#971), beside how old the payload is — the two halves of
          "is this still worth deciding?".

          Rendered only when the host reports one. An absent
          `expires_at_millis` means the host does not have deadlines, NOT that
          this card has none, so the console shows nothing rather than
          computing a deadline nothing would enforce: an operator who acted on
          an invented "in 3h" would be refused.

          Wording and tone both come from `approvalDeadline` (#1403), so what
          this says is testable as a string rather than only as rendered output
          — the same split `payloadAge` above already uses. The tone is not
          decoration: this line is the only thing on the card that says the
          decision will be taken *for* the operator if they keep scrolling, and
          it used to say it in the same grey as everything else. Amber is what
          the rest of the console already means by "parked until a person acts"
          (`--status-blocked`), and the passed state borrows the failed token
          because a deadline that ran out is a terminal no. */}
      {typeof a.expires_at_millis === "number" && (
        <>
          <span aria-hidden>·</span>
          <span className={deadlineToneClass(deadline.tone)}>
            {deadline.text}
          </span>
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
 * The only workflow approval shape that carries both parts of the run address.
 *
 * `workflow_run_id` names a run but the console route also needs its workflow.
 * Native `workflow.approve` effects carry that id as a **top-level summary
 * field** (`ApprovalSummary.workflow_id`), projected by the host from the raw
 * parked effect rather than from the display payload — the payload is redacted
 * and role redaction (#618) strips it from a member reader, and the run link
 * must survive for the member holding the stalled workflow up. A tool call
 * parked by a workflow carries neither field. Never infer the workflow from a
 * run id: it has no global namespace and could send the operator to a
 * different workflow.
 */
function workflowIdForApproval(approval: ApprovalSummary): string | null {
  if (approval.kind !== "workflow.approve") return null;
  const workflowId = approval.workflow_id;
  return typeof workflowId === "string" && workflowId.length > 0 ? workflowId : null;
}

/**
 * How an approval's deadline is typeset, by tone (#1403).
 *
 * Weight as well as colour in both loud arms, so the distinction survives
 * greyscale and the colour-vision deficiencies red/amber is worst for. `normal`
 * returns nothing at all and inherits the meta line's muted grey, which keeps
 * the quiet case exactly as it shipped — the emphasis is only worth anything if
 * most cards do not have it.
 *
 * Exported for the board card (#1891), which paints the deadline without the
 * rest of {@link ApprovalMeta}: a `w-65` column has no room for the origin
 * pills, and "Open the card" on the card you are already looking at is the same
 * redundancy `OutputLinkRow` exists to avoid. The *tone* is not the board's to
 * re-decide, though — a surface that invented its own amber would be the one
 * place a passing deadline looked routine.
 */
export function deadlineToneClass(tone: DeadlineTone): string | undefined {
  if (tone === "passed") return "font-medium text-status-failed-text";
  if (tone === "soon") return "font-medium text-status-blocked-text";
  return undefined;
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
          Details hidden by your role. An admin can see what this approval will
          do and decide it.
        </span>
      </div>
    );
  }

  // A named action can still be decided from its headline. The generic
  // fallback cannot: without a payload it otherwise leaves the operator with
  // no fact at all about what is being approved (#1419).
  if (
    lines.length === 0 &&
    approvalAction(approval) === "Do something that needs your sign-off"
  ) {
    return (
      <p className="text-xs text-muted-foreground">
        No further details were supplied.
      </p>
    );
  }

  if (lines.length === 0) return null;

  const clampable =
    lines.length > PREVIEW_LINES ||
    lines.some((l) => l.value.length > PREVIEW_VALUE_CHARS);
  const shown = expanded || !clampable ? lines : lines.slice(0, PREVIEW_LINES);

  return (
    <div className="rounded-lg border bg-muted/40 px-3 py-2">
      <div
        className={cn(
          "space-y-1 font-mono text-xs break-all whitespace-pre-wrap",
          clampable && !expanded && "line-clamp-3",
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
          {expanded ? (
            <ChevronUp className="size-3" />
          ) : (
            <ChevronDown className="size-3" />
          )}
          {expanded ? "Show less" : "Show everything"}
        </button>
      )}
    </div>
  );
}

/** A resolved chat destination for an approval's host-side thread id. */
export interface ApprovalThreadLink {
  channelId: string;
  /** A channel is written with `#`; a direct message is written as a name. */
  label: string;
}

/**
 * Resolve an approval's host thread id into the human-facing chat destination.
 *
 * The host calls desk channels by their desk id, while direct messages use an
 * agent id. `channelIdForThread` is the one place that bridges those two id
 * schemes; keeping this join here prevents the Approvals page from linking a
 * DM to a URL no chat channel owns.
 */
export function approvalThreadLink(
  approval: ApprovalSummary,
  /**
   * The company's desks, or `null` when the read failed and the topology is
   * unknown.
   *
   * The two were one value until desk fabrication was removed: an empty answer
   * used to be replaced by `defaultDesks()`, so `[]` could only mean a failed
   * read. It now means what it says — a company with no desks — and the
   * failure it used to stand for is spelled `null`, because the `#general`
   * label below must be withheld from one and not from the other.
   */
  desks: Desk[] | null,
  members: TeamMember[],
): ApprovalThreadLink | null {
  if (!approval.thread) return null;
  const known = desks ?? [];
  const channelId = channelIdForThread(approval.thread, known, members);
  if (!channelId) return null;

  // Looked up by the **resolved channel**, not by the raw thread id. They are
  // the same string for an ordinary desk, and deliberately different when a
  // blueprint desk is grandfathered onto the company-wide line: an approval
  // raised under `main` resolves to that desk's channel, and asking for a desk
  // called `main` would find nothing and label it "Origin unavailable" — on a
  // conversation whose transcript is right there on screen.
  const desk = known.find((candidate) => candidate.id === channelId);
  if (desk) return { channelId, label: `#${desk.channel}` };

  // The built-in `#general` channel (issue #1743), which is deliberately in no
  // desk list — so the scan above can never name it, and an approval raised on
  // the company's main line resolved to a channel and then failed to find a
  // label, leaving "Origin unavailable" on the one channel every company has.
  // After the desk scan, deliberately: a blueprint desk that authored one of
  // the General ids keeps its own name, exactly as `channelIdForThread` keeps
  // it its own thread.
  //
  // Guarded on the topology being *known* rather than on the list being
  // non-empty. A failed read must not be guessed at — `ChatView` surfaces the
  // error and renders no rail, so a link into it would land nowhere — but a
  // company that genuinely declares no desks still has `#general`, and that is
  // the one channel every company has. While an empty answer was overwritten
  // with `defaultDesks()` the two cases were the same value, and reading the
  // length was the only test available; now the failure says `null`.
  if (channelId === MAIN_THREAD_ID && desks !== null) {
    return { channelId, label: `#${GENERAL_CHANNEL}` };
  }

  // Matched through `dmThreadId`, not against the bare id: a DM for a teammate
  // whose id is a General spelling records its thread as `dm:<id>`, and a raw
  // comparison returned `null` — so the Approvals page called the origin
  // unavailable for a conversation it could perfectly well link to (#1743).
  const member = memberForThread(members, approval.thread);
  return member ? { channelId: dmChannelId(member), label: member.name } : null;
}

/**
 * Read the small amount of chat topology the Approvals page needs to link a
 * parked request back to its conversation. An unreadable or older route simply
 * leaves the existing card intact: an unresolved thread must not be guessed.
 *
 * The desks and roster are the slow-moving half of the join; the approvals
 * themselves arrive on every poll. Keeping the two apart is what lets a
 * freshly arrived card on an already-known thread get its "Asked in" link:
 * an effect that rebuilt the map only when the *set of thread ids* changed
 * would skip a new approval that shares its thread with one already pending.
 */
export function useApprovalThreadLinks(
  client: OpenCompanyClient,
  company: string | null,
  approvals: ApprovalSummary[],
  /**
   * True while the queue is interaction-held (#1593): a desk/roster read that
   * resolves mid-hold must not swap an "Asked in" link into a card the
   * operator is aiming at — the link wraps differently from "Origin
   * unavailable" and can shift the decide buttons. Applied when the hold
   * releases, exactly like `useAskerNames`. Defaults to off so the other
   * callers (chat, workflows) keep resolving links immediately.
   */
  holding = false,
): Map<string, ApprovalThreadLink> {
  const threadKey = useMemo(
    () =>
      Array.from(
        new Set(approvals.map((approval) => approval.thread).filter(Boolean)),
      )
        .sort()
        .join(","),
    [approvals],
  );
  const [topology, setTopology] = useState<{
    /** `null` when the desks read failed — see `approvalThreadLink`. */
    desks: Desk[] | null;
    members: TeamMember[];
  } | null>(null);
  // Topology that resolved during a hold. `null` means nothing pending; an
  // empty topology is a real answer and must not be mistaken for "nothing
  // arrived".
  const pending = useRef<{
    desks: Desk[] | null;
    members: TeamMember[];
  } | null>(null);
  // Live is read inside the async topology read, which must not close over a
  // stale render — the same ref pattern `useAskerNames` uses.
  const holdingRef = useRef(holding);
  holdingRef.current = holding;

  useEffect(() => {
    if (!threadKey) {
      setTopology(null);
      return;
    }
    let live = true;
    void Promise.all([
      // The host's answer, taken as given — the same rule ChatView and
      // AppShell now follow. An empty list is a company with no desks, and an
      // approval raised on its `main` thread still resolves to `#general`. It
      // used to be swapped for `defaultDesks()`, which resolved approvals to
      // fabricated channels the rail no longer shows.
      //
      // A *failed* read is `null`, not `[]`: unknown, and not to be guessed at.
      client
        .listDesks(company)
        .then((dtos) => dtos.map(deskFromDto))
        .catch(() => null),
      client.listTeam(company).catch(() => []),
    ]).then(([desks, roster]) => {
      if (!live) return;
      const next = { desks, members: roster.map(fromDto) };
      if (holdingRef.current) pending.current = next;
      else setTopology(next);
    });
    return () => {
      live = false;
    };
  }, [client, company, threadKey]);

  // Topology that resolved during a hold applies the moment it releases.
  // Without this a read that lands mid-interaction stays invisible until the
  // next thread-key change, which could be never.
  useEffect(() => {
    if (holding) return;
    if (pending.current !== null) {
      const next = pending.current;
      pending.current = null;
      setTopology(next);
    }
  }, [holding]);

  return useMemo(() => {
    if (!topology) return new Map();
    return new Map(
      approvals.flatMap((approval) => {
        const link = approvalThreadLink(
          approval,
          topology.desks,
          topology.members,
        );
        return link ? [[approval.id, link] as const] : [];
      }),
    );
  }, [approvals, topology]);
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
      <legend className="px-1 text-xs text-muted-foreground">
        If you approve
      </legend>
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
              onChange({
                kind: "tool",
                expiresInMillis: GRANT_DURATIONS[0].millis,
              })
            }
            className="size-3.5 accent-primary"
          />
          <span>Let {askerLabel(a, askerNames)} use this tool for</span>
          <select
            value={
              scope.kind === "tool"
                ? scope.expiresInMillis
                : GRANT_DURATIONS[0].millis
            }
            disabled={scope.kind !== "tool"}
            onChange={(e) =>
              onChange({
                kind: "tool",
                expiresInMillis: Number(e.target.value),
              })
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
          It won't ask again for this tool until then — with any arguments. You
          can take it back from Standing permissions at any time.
        </p>
      )}
    </fieldset>
  );
}

/** The matching, opt-in scope for a refusal (issue #1458). */
export function DeclineScopeControl({
  approval: a,
  scope,
  onChange,
  disabled,
}: {
  approval: ApprovalSummary;
  scope: GrantScope;
  onChange: (scope: GrantScope) => void;
  disabled?: boolean;
}) {
  if (!a.broadly_deniable) return null;
  const name = `decline-scope-${a.id}`;
  return (
    <fieldset
      disabled={disabled}
      className="rounded-lg border bg-muted/30 px-3 py-2 text-sm disabled:opacity-60"
    >
      <legend className="px-1 text-xs text-muted-foreground">
        If you decline
      </legend>
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
            onChange={() =>
              onChange({
                kind: "tool",
                expiresInMillis: GRANT_DURATIONS[0].millis,
              })
            }
            className="size-3.5 accent-primary"
          />
          <span>Don't ask again for this tool for</span>
          <select
            value={
              scope.kind === "tool"
                ? scope.expiresInMillis
                : GRANT_DURATIONS[0].millis
            }
            disabled={scope.kind !== "tool"}
            onChange={(e) =>
              onChange({
                kind: "tool",
                expiresInMillis: Number(e.target.value),
              })
            }
            aria-label="How long this refusal lasts"
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
          It won't ask again for this tool until then. You can take it back from
          Standing permissions at any time.
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
function askerLabel(
  a: ApprovalSummary,
  askerNames: Map<string, string>,
): string {
  // A native `workflow.approve` gate carries no agent — the grant's subject is
  // the workflow itself (issue #1098), so naming a "teammate" would tell the
  // operator the wrong grantee right as they pick the broader scope.
  if (a.workflow_id != null && a.workflow_id !== "") return "this workflow";
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
 *
 * `holding` defers the roster names the same way `useStandingGrants` defers
 * grants (#1593): the labels they populate sit ABOVE the queue (the standing
 * permissions section), and a name landing mid-interaction (raw id → display
 * name) can change a card's wrapping and shift every approve/decline control
 * under the operator's pointer. Names that arrive during the hold are applied
 * the moment it releases.
 */
export function useAskerNames(
  client: OpenCompanyClient,
  company: string | null,
  approvals: ApprovalSummary[],
  /** True while the queue is interaction-held (#1593). Defaults to off so the
   *  other callers (chat, workflows) keep applying names immediately. */
  holding = false,
): Map<string, string> {
  const [names, setNames] = useState<Map<string, string>>(new Map());
  // The newest names while the queue is held. `null` means nothing pending; an
  // empty map is a real answer and must not be mistaken for "nothing arrived".
  const pending = useRef<Map<string, string> | null>(null);
  // Live is read inside the async roster read, which must not close over a
  // stale render — the same ref pattern `useStableList` uses for `live`.
  const holdingRef = useRef(holding);
  holdingRef.current = holding;
  // Keyed on the set of asker ids rather than on `approvals` itself: the feed
  // hands us a fresh array on every poll, and depending on the array would
  // refetch the roster every few seconds for a roster that rarely changes.
  const askerKey = useMemo(
    () =>
      Array.from(
        new Set(
          approvals.map((a) => a.agent).filter((id): id is string => !!id),
        ),
      )
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
      const next = new Map(roster.map((m) => [m.id, m.name?.trim() || m.role]));
      if (holdingRef.current) pending.current = next;
      else setNames(next);
    })();
    return () => {
      live = false;
    };
  }, [client, company, askerKey]);

  // Names that arrived during a hold render the moment it releases. Without
  // this a roster resolved mid-interaction stays invisible until the next
  // asker-key change, which could be never.
  useEffect(() => {
    if (holding) return;
    if (pending.current !== null) {
      const next = pending.current;
      pending.current = null;
      setNames(next);
    }
  }, [holding]);

  return names;
}
