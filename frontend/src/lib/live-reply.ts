// Whose live `agent_reply` frame the shell must drop, and whose it must render.
//
// Extracted from `AppShell` because it is the single highest-risk rule in the
// detached-chat change (issue #983) and it fails in opposite, equally invisible
// ways. Suppress when you should not and the operator's reply never appears at
// all; render when you should not and every reply appears twice. Neither throws,
// neither shows up in a type, and both look like "the chat is behaving oddly".
// As a closure over a ref it could only be exercised by driving the whole shell;
// as a rule with a name it can be asserted transition by transition.

import { hostMessageId, isHostMessageId, type ChatMessage } from "./chat";

/**
 * Whether a live `agent_reply` frame is a duplicate of something already in
 * the recent tail of a transcript — never render it if so (PR #2052 review).
 *
 * Two checks, because one alone fails in a different direction each:
 *
 * - **Identity first.** Every frame carries a durable id via its own `seq`
 *   (`hostMessageId(String(event.seq))`, the same value `liveReplyIdentity`
 *   spreads into the row it renders as `messageId`). A row already bearing
 *   that exact id is unambiguously this same event, rendered before —
 *   whatever its content, it is always a duplicate.
 * - **Content, but only against an unreconciled row.** The operator's own
 *   turn is rendered locally, immediately, under an ephemeral `m<n>` id from
 *   the awaited POST response; the backend also journals and broadcasts that
 *   same reply, and the SSE echo can arrive first, mid-await, before the
 *   POST resolves and reconciles that row to its durable id. In that narrow
 *   window identity cannot recognise the echo as the same message — only
 *   content can. Scoping this check to `!isHostMessageId(m.id)` is what
 *   keeps it from over-firing once that window closes: two *different*
 *   events that merely share wording — an operator repeating the same
 *   ambiguous `@name` produces two B-101 notices with identical text — both
 *   carry durable ids from the start, so neither is `!isHostMessageId`, and
 *   content matching never gets a chance to conflate them. Before this
 *   distinction existed, content matching alone silently dropped the second
 *   one outright, which is a stronger failure than a duplicate render: this
 *   check exists to prevent noise, not to swallow a genuinely new refusal.
 */
export function isDuplicateLiveReply(
  recentTail: readonly Pick<ChatMessage, "id" | "from" | "text">[],
  event: { seq: number; text: string },
  from: ChatMessage["from"],
): boolean {
  const eventId = hostMessageId(String(event.seq));
  return recentTail.some(
    (m) => m.id === eventId || (!isHostMessageId(m.id) && m.from === from && m.text === event.text),
  );
}

/**
 * The one field {@link PendingSyncPosts.capture} needs off a live `agent_reply`
 * frame. A local shape rather than importing the hook's event type, matching
 * {@link OpenTurnRow} / {@link OpenRunRow} below — this module stays testable
 * without dragging in `hooks/use-events`.
 */
export interface LiveReplyFrame {
  chatId: string;
  /**
   * The frame's `agentId`, when its type carries one (`AgentReplyEvent`
   * always does). Read only by {@link PendingSyncPosts.ended}, to tell a held
   * system-attributed frame the settled response duplicates from one it
   * never will — see that method's doc for why the distinction only matters
   * for this one attribution.
   */
  agentId?: string;
  /**
   * The frame's own reply text, when its type carries one (`AgentReplyEvent`
   * always does). Read only by {@link PendingSyncPosts.ended}, compared
   * against the settled response's own reply text(s) for the same reason
   * `agentId` is.
   */
  text?: string;
}

/**
 * Whose voice a live `agent_reply` frame renders as: the runtime's own
 * centred system pill, or a named teammate's bubble (issue #101 / B-101
 * review, PR #2052).
 *
 * `SYSTEM_AUTHOR` on the Rust side (`crate::ports::SYSTEM_AUTHOR`, the string
 * `"system"`) is the one `agentId` that names the runtime itself rather than
 * a roster agent — a mention-ambiguity notice is journaled under it precisely
 * so a reader can tell "the runtime is reporting its own refusal" from "a
 * teammate replied". `fromHistory` (`lib/chat.ts`) already makes this same
 * distinction for a rehydrated row (`entry.author === "system"`); this is the
 * **live**-path twin of that rule.
 *
 * Extracted for the same reason every rule in this module is (see the file
 * doc above): inlined as a closure inside `renderAgentReply`, a regression to
 * a hard-coded `"company"` shows up nowhere but a live screenshot taken
 * during a detached send, between the SSE frame landing and the next history
 * reload silently correcting it — and no test would ever see it (tinysweeper
 * review). Named, it can be asserted directly.
 */
export function liveReplyAttribution(agentId: string): "system" | "company" {
  return agentId === "system" ? "system" : "company";
}

/**
 * The threads with a **synchronous** chat POST in flight.
 *
 * ## The rule, and why it is not "a turn is running"
 *
 * The host journals an `AgentReply` for the operator's own turn too, and pushes
 * it over SSE. When the console is *awaiting* that turn's POST, the awaited
 * response is the authoritative copy (it carries the folded step timeline), and
 * the live frame is a duplicate that would double the bubble — so it is dropped.
 *
 * A **detached** turn inverts that. Its POST answered `202` immediately and is
 * never going to carry a reply, so the live frame is not a duplicate of the
 * answer — it *is* the answer. Suppressing it there means the reply never
 * arrives on screen, which is a strictly worse failure than the double bubble
 * this rule exists to prevent.
 *
 * So membership means "a POST is in flight that will itself deliver the reply",
 * which is why {@link detached} removes a thread the moment the `202` lands even
 * though its turn is still very much running.
 *
 * ## A POST has three outcomes, not two
 *
 * A frame held by {@link capture} is disposed of by whichever outcome the POST
 * reaches, and each outcome disposes of it differently *because the question is
 * always the same one*: did anything else already put this reply on screen?
 *
 * | Outcome | Reached by | Held frames | Because |
 * |---|---|---|---|
 * | **resolved** | {@link ended} | discarded | the awaited body carried the reply and the caller has already rendered it, folded steps and all — a held frame is that same reply a second time |
 * | **detached** | {@link detached} | released | the `202` carried no reply and never will; the stream is the delivery path, so the held frame is the *only* copy of the answer |
 * | **failed** | {@link failed} | released | the POST threw, so nothing was rendered — and the turn is very likely still running on the host, which makes the stream the only delivery path here too |
 *
 * The third row is the one that is easy to miss and the reason it exists at all
 * (issue #1000). A throw is not a settled turn: the request is what died, not
 * the work. The gateway cutting a 120s request, a laptop losing its network, the
 * tenant proxy timing out — the host keeps running the turn and journals its
 * reply, which arrives over SSE like any other. Routing that case through
 * {@link ended} discards a frame that nothing else will ever render, which is
 * the exact silent loss this class was extracted to prevent, merely moved onto
 * the error path — and the error path is the one the detached design exists for.
 *
 * So the split is by *what happened to the POST*, never by widening `ended` to
 * mean "the POST is over". Two of the three outcomes leave a turn running.
 *
 * ## The fourth case: a POST that never settles
 *
 * A request that neither resolves nor throws — no response headers, no error,
 * still pending minutes later — reaches none of the three methods above. Its
 * thread stays suppressed and its held frames stay held, so a reply that
 * arrives in that window is not rendered until something else puts it on screen
 * (the turn's terminal `chat/history` re-read, or the next hydration of the
 * channel).
 *
 * This is written down rather than fixed on purpose. It is **not** a
 * regression: the code before this class dropped such a frame outright, so the
 * outcome on screen is identical and the durable record is unchanged. And the
 * fix that suggests itself — expire the hold after N seconds — is precisely the
 * window-based reasoning this class replaced. A timer cannot tell a hung POST
 * from a slow one, so it would reintroduce, on a timer's schedule, the drop
 * that holding by identity removed. If this case ever needs closing, close it
 * with a fact rather than a duration: the request's own `AbortSignal` firing,
 * which is an outcome and belongs in {@link failed}.
 *
 * It is reachable in production — the hosting manager's proxy buffers a whole
 * upstream body before it will build a response, which is why `/events` hangs
 * past two minutes on a hosted tenant (`opencompany-microservice#23`).
 */
export class PendingSyncPosts<F extends LiveReplyFrame = LiveReplyFrame> {
  private readonly threads = new Set<string>();
  /**
   * Frames {@link capture} held back because their thread's POST shape was
   * still unknown when they arrived, keyed by thread and kept in arrival
   * order. Resolved for good — never left to expire — by whichever of
   * {@link ended} / {@link detached} / {@link failed} the POST turns out to
   * reach.
   */
  private readonly held = new Map<string, F[]>();

  /**
   * A chat POST has gone out on this thread.
   *
   * Suppression starts here, before the shape of the answer is known, and that
   * is the safe default in both directions: a synchronous turn's echo arrives
   * mid-await and must be dropped, while a detached turn's `202` comes back in
   * milliseconds and lifts the suppression long before its reply is written.
   */
  started(threadId: string): void {
    this.threads.add(threadId);
  }

  /**
   * The host answered `202`: accepted, not answered (issue #983).
   *
   * The turn is still running — this is emphatically not `ended` — but this POST
   * has stopped being the delivery path, so the stream takes over.
   *
   * Returns whatever {@link capture} held for this thread, oldest first, so the
   * caller can render it now. This is the fix for the race the boolean alone
   * could not close: `onSendStart` arms suppression synchronously, but nothing
   * makes the browser learn the `202`'s shape before a fast turn's SSE frame
   * already arrived — a detached echo brain can answer in milliseconds, well
   * inside the round trip. A frame landing in that window used to be dropped
   * outright, which is a silent, permanent loss of the operator's only reply.
   * Holding it and handing it back here — identity by thread, not a timer —
   * closes the window instead of narrowing it: no frame captured while a
   * thread's shape was unknown is ever thrown away, no matter when it lands
   * relative to the `202`.
   */
  detached(threadId: string): F[] {
    return this.release(threadId);
  }

  /**
   * The chat POST **threw**: no response body, nothing rendered (issue #1000).
   *
   * Not {@link ended}, and the distinction is the whole point of this method.
   * `ended` may discard held frames because the awaited body already put that
   * reply on screen; a throw put nothing on screen, so there is no copy for a
   * held frame to duplicate. Discarding here loses the reply outright.
   *
   * And the turn is very likely still running. That is the premise of the whole
   * detached design (issue #983): the work outlives the request, so a gateway
   * cutting the connection at 120s kills the response and nothing else — the
   * host finishes the turn and journals the reply, which arrives over SSE like
   * any other. From the moment the request dies the stream is the only delivery
   * path this console has, exactly as it is after a `202`.
   *
   * So this behaves like {@link detached}: hand back whatever {@link capture}
   * held for this thread, oldest first, and lift the suppression. The caller
   * still tells the operator the request failed — a reply that lands later does
   * not make the failure untrue, and the two facts are not in competition.
   */
  failed(threadId: string): F[] {
    return this.release(threadId);
  }

  /**
   * The synchronous POST resolved with a body; `responseTexts` is every reply
   * line that body itself carried.
   *
   * A held frame attributed to the operator's own turn (any `agentId` other
   * than `SYSTEM_AUTHOR`) is discarded unconditionally, as it always was: it
   * is, by the same reasoning as the class doc, a live echo of that same
   * reply, and the awaited response is authoritative for it.
   *
   * **A held *system*-attributed frame needs `responseTexts` to answer the
   * same question, because the blanket assumption is false for it** (Codex
   * review, PR #2052). Some system-authored lines ARE folded into
   * `channel_responses` the same way any reply is — `system_notice`'s
   * approval-overflow and `"Acknowledged."` fallback among them — and for
   * those the assumption above still holds. Others never are: B-101's
   * mention-ambiguity note is deliberately journaled outside that pipeline
   * (`post_mention_ambiguity_note`'s own doc: "Journaled, not returned in the
   * POST response"), specifically so it reaches an API poster who renders no
   * chip at all. Discarding every held system frame here would silently
   * swallow that note on a synchronous send; rendering every one instead
   * would double-render whichever ones the response DOES carry, next to the
   * identical `"company"` bubble `ChatView` appends from `responseTexts`
   * itself. So a held system frame is discarded only when its text is
   * present in `responseTexts` — the response already carries it — and
   * released, for the caller to render, when it is not — the response never
   * will.
   *
   * Scoped to a POST that *resolved*. A POST that threw resolved nothing and
   * belongs in {@link failed}; sending it here discards a reply the console is
   * never going to be handed again.
   */
  ended(threadId: string, responseTexts: readonly string[] = []): F[] {
    this.threads.delete(threadId);
    const held = this.held.get(threadId) ?? [];
    this.held.delete(threadId);
    return held.filter(
      (frame) => frame.agentId === "system" && !responseTexts.includes(frame.text ?? ""),
    );
  }

  /**
   * Lift this thread's suppression and hand back what was held for it, oldest
   * first — the shared half of {@link detached} and {@link failed}.
   *
   * The two stay separate methods rather than one `postOver` because they are
   * separate facts about the turn, and a caller that cannot say which one it
   * means is a caller that has not decided. `detached` has a turn id and a row
   * to poll; `failed` has neither, and a working row armed from it would be a
   * spinner with nothing to take it down.
   */
  private release(threadId: string): F[] {
    this.threads.delete(threadId);
    const frames = this.held.get(threadId) ?? [];
    this.held.delete(threadId);
    return frames;
  }

  /** Whether a live `agent_reply` for this thread would be a duplicate. */
  suppressesLiveReply(threadId: string): boolean {
    return this.threads.has(threadId);
  }

  /**
   * Route one live `agent_reply` frame: render it now, or hold it because this
   * thread's POST has not yet told the console what it delivers.
   *
   * Returns `true` when the frame was held — the caller must not render it,
   * whichever of {@link ended} / {@link detached} / {@link failed} the POST
   * reaches will dispose of it — and `false` when
   * there is nothing pending on this thread and the frame is the caller's to
   * render immediately, same as before this thread ever posted.
   *
   * This is the only place a frame's fate is decided, and it decides by
   * identity — is this thread's POST still unresolved — never by how long it
   * has been unresolved. See {@link detached} for why that distinction is the
   * whole fix.
   *
   * Holds a system-attributed frame exactly like any other while a POST is in
   * flight — earlier code exempted it here instead (Codex review, PR #2052),
   * which fixed the case that motivated it (the ambiguity note lost to
   * `ended`'s blanket discard) but broke a different one: a `system_notice`
   * fallback the response body DOES carry then rendered twice, once from this
   * bypass and once from `ChatView`'s own append of `responseTexts`. See
   * {@link ended}'s doc for why the fix belongs on release, where the
   * response's own text is available to reconcile against, not on capture,
   * where it never was.
   */
  capture(frame: F): boolean {
    if (!this.suppressesLiveReply(frame.chatId)) return false;
    const queue = this.held.get(frame.chatId);
    if (queue) queue.push(frame);
    else this.held.set(frame.chatId, [frame]);
    return true;
  }
}

/**
 * One chat turn that has been accepted but has not settled (issue #983).
 *
 * The shell holds these **per thread, as an ordered list** — a thread can have
 * a running turn and a queued one behind it at once (the per-company serial
 * lock admits exactly that), and every one of them has a reply the operator is
 * eventually shown. The first entry is the one the working indicator reads
 * where it renders; the poll watches the whole list and drains oldest-first.
 * Declared here rather than in the shell so the fold below and the views can
 * share it without a type-cycle.
 */
export type OpenTurn = {
  /**
   * The durable row to poll. Absent when the host could not mint one — the turn
   * still ran, and the indicator still shows, it just cannot be watched.
   */
  turnId?: string;
  /**
   * The row is still `pending`: accepted, but waiting on the per-company serial
   * lock rather than working. Drives the indicator's wording.
   */
  queued: boolean;
  /**
   * The **desk** this turn is in — the host thread id, never the map key.
   *
   * The map is keyed per thread (`turnStateKey`), so its key can be a composite
   * like `engineering#41`. Consumers that need to talk to the *host* — the
   * settle poll re-reads a desk's history — must use this, because there is no
   * desk called `engineering#41` and asking for one silently recovers nothing
   * (Codex review on #2042).
   *
   * **Required, so the compiler proves every insertion site carries it.** It was
   * optional for one commit, with the desk recovered from the key by stripping a
   * trailing `#<digits>`. That parser is lossy in exactly the case it claimed to
   * handle — a desk genuinely named `c#4` parses to `c` — and a fallback that is
   * usually right is worse than none here, because the failure is a silent
   * history read against a desk that does not exist (CodeRabbit on #2044).
   */
  chatId: string;
};

/** The per-thread rows the fold produces — the same shape as {@link OpenTurn}. */
export type OpenTurnRow = OpenTurn;

/** The shape {@link openTurnsFromRuns} reads — the run rows' relevant fields. */
export interface OpenRunRow {
  id: string;
  chatId?: string;
  /** The thread within `chatId`, when the host resolved one. */
  threadRoot?: number;
  status: string;
}

/**
 * The key the shell's live-turn maps — open turns, live steps, receipts — are
 * held under.
 *
 * The **thread**, not the channel. All three used to key on the chat id, which
 * names only the channel; a channel has held many threads since #1890, so two
 * concurrent turns in one channel shared a slot. The visible cost was not a
 * mixed-up list but a silent one: unable to tell whose turn was running,
 * `ChatView` suppressed the working indicator for the whole channel whenever
 * any thread was open, and a turn the host was actively running showed nowhere
 * at all.
 *
 * Falls back to the chat id when the host resolved no root — a card dispatch, a
 * workflow node, or a row written before the host carried it. That is also
 * exactly the identity the maps had before, so an un-upgraded host keeps the
 * previous behaviour rather than losing its indicator.
 */
export function turnStateKey(chatId: string, threadRoot?: number): string {
  return threadRoot === undefined ? chatId : `${chatId}#${threadRoot}`;
}


/**
 * Folds the open run rows into the per-thread turn lists the working indicator
 * and the poll read (issue #983).
 *
 * This is the reload leg, and it is the thing that was impossible before the
 * turn became durable: a console that never saw the POST asks which turns are
 * open and re-arms the indicator from the answer, instead of showing a
 * settled-looking transcript with an answer still on its way.
 *
 * Two rules earn their own assertions. A run at a **card** is a dispatch, not a
 * chat turn, and owns no thread's indicator — so a row with no conversation is
 * skipped rather than defaulted somewhere. And `pending` versus `running` is
 * carried through rather than flattened, because "queued behind other turns" and
 * "working" are different things to tell an operator, and the serial lock makes
 * the first one common.
 *
 * Every open row is kept, running ones first. A thread can hold a running turn
 * and a queued one behind it, and both have a reply the operator is waiting on;
 * the running one merely names the indicator, which is what the ordering — not
 * a discard — expresses. Dropping the queued sibling was the bug where a second
 * detached send on a loaded-away console left a reply nobody was watching for
 * (issue #1000).
 */
export function openTurnsFromRuns(runs: readonly OpenRunRow[]): Record<string, OpenTurnRow[]> {
  const byThread = new Map<string, OpenTurnRow[]>();
  const seen = new Set<string>();
  for (const run of runs) {
    if (!run.chatId || seen.has(run.id)) continue;
    seen.add(run.id);
    const queued = run.status === "pending";
    const key = turnStateKey(run.chatId, run.threadRoot);
    const list = byThread.get(key);
    const row = { turnId: run.id, queued, chatId: run.chatId };
    if (list) list.push(row);
    else byThread.set(key, [row]);
  }
  const open: Record<string, OpenTurnRow[]> = {};
  for (const [chatId, rows] of byThread) {
    // Stable sort, so same-status rows keep the store's order: running first,
    // queued behind — whichever order the store listed them in. The head is
    // what the working indicator reads.
    rows.sort((a, b) => (a.queued === b.queued ? 0 : a.queued ? 1 : -1));
    open[chatId] = rows;
  }
  return open;
}

/**
 * Merges one leg's turns into the shell's map without ever replacing a turn
 * another leg already registered (issue #1000).
 *
 * `onSendDetached` appends from the POST's own answer; the fold above arms
 * from `listRuns` after a reload or a failed POST. The two can race on the
 * same turn — a reload mid-POST, a re-arm landing after the 202 — so this
 * collapses a repeated `turnId` onto one entry, with the incoming `queued`
 * reading winning because a store answer is newer than the moment of accept.
 * The head of a thread's list is untouched by an incoming shorter list: arms
 * add, they never reorder or evict.
 */
/**
 * Is another turn still running on `threadId`, ignoring the one that just
 * settled?
 *
 * The question the console asks before clearing a thread's live tool rows. It
 * takes `settledTurnId` explicitly rather than trusting the map to have
 * dropped it already, because the caller's view of `openTurns` is a ref that
 * an effect refreshes *after* React commits — and a history read that resolves
 * before that commit would still see the settled turn and skip a clear it
 * should have made (PR #1904 review).
 *
 * Excluding it by id is right whichever way the race falls: on a stale map the
 * filter removes the turn that is already over, and on a fresh one it removes
 * nothing. A turn opened since is present either way, which is the case that
 * must block the clear.
 */
export function hasOtherOpenTurns(
  openTurns: Readonly<Record<string, readonly OpenTurn[]>>,
  threadId: string,
  settledTurnId?: string,
): boolean {
  const turns = openTurns[threadId] ?? [];
  // An id-less entry (`onSendDetached`'s row for a turn the host could not
  // mint an id for) can never be watched or settled by the poll, so counting
  // it here would block this clear forever (CodeRabbit, PR #1904 review).
  return turns.some(
    (turn) => turn.turnId && (!settledTurnId || turn.turnId !== settledTurnId),
  );
}

export function mergeOpenTurns(
  existing: Record<string, OpenTurn[]>,
  incoming: Record<string, OpenTurn[]>,
): Record<string, OpenTurn[]> {
  const out: Record<string, OpenTurn[]> = { ...existing };
  for (const [threadId, turns] of Object.entries(incoming)) {
    const current = out[threadId] ?? [];
    const merged = [...current];
    const indexOfTurn = new Map<string, number>(
      merged.flatMap((t, i) => (t.turnId ? [[t.turnId, i] as [string, number]] : [])),
    );
    for (const turn of turns) {
      // A turn with no row cannot be matched against anything, so it is always
      // its own entry: two id-less turns are two turns, not one.
      const at = turn.turnId ? indexOfTurn.get(turn.turnId) : undefined;
      if (at === undefined) {
        if (turn.turnId) indexOfTurn.set(turn.turnId, merged.length);
        merged.push(turn);
      } else {
        merged[at] = { ...merged[at], ...turn };
      }
    }
    out[threadId] = merged;
  }
  return out;
}
