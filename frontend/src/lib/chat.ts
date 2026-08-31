import type {
  AttachmentDto,
  ChatHistoryMessageDto,
  ChatMentionDto,
  TurnStep,
} from "@/api/types";

/**
 * The company's main line, by thread id.
 *
 * Mirrors the host's `MAIN_THREAD_ID` (`src/server/chat_history.rs`), whose
 * `is_general_chat` folds this id, `"General"` and the **empty string** into a
 * single conversation. Named rather than spelled inline because
 * {@link dispatchMarkerPlacement} has to resolve an empty origin to *this*
 * thread rather than to nothing — see the rules on that function.
 */
export const MAIN_THREAD_ID = "main";

/**
 * The name of the company-wide channel, rendered after the `#`.
 *
 * Mirrors the host's `GENERAL_CHANNEL` (`src/server/ops/language.rs`), the same
 * way {@link MAIN_THREAD_ID} mirrors its `MAIN_THREAD_ID`.
 */
export const GENERAL_CHANNEL = "general";

/**
 * Does this id name the built-in `#general` channel?
 *
 * Mirrors the host's `is_general_chat` (`src/server/chat_history.rs`), which
 * has folded four spellings into one conversation since issue #65: the empty
 * string, `main` (what this console addresses the line as), `General` (the
 * name the host attributes an unaddressed turn to), and `general`. The host
 * reserves every one of them — a desk cannot be created with any of them as
 * its id — so this is a closed set, not a guess.
 *
 * **Case-folded and nothing else.** The host compares with
 * `eq_ignore_ascii_case` against the string exactly as journaled, so it does
 * not trim — and neither may this, or the two disagree about the same id.
 * Trimming here was strictly worse than being strict: an API client posting
 * `chat: "  Main  "` has that spelling journaled verbatim, so the console
 * rendered the live reply in `#general` while `chat/history?desk=main` did not
 * return it, and the message vanished on the next reload. A live frame that
 * never lands is a message the operator has not seen; one that lands and then
 * disappears reads as data loss.
 *
 * Lives here rather than in `lib/desks.ts` — where it used to — because it is a
 * fact about chat *addressing*, like {@link MAIN_THREAD_ID} beside it, and
 * because `dispatchMarkerPlacement` below has to apply it. `lib/desks.ts`
 * re-exports it, so nothing that reads it had to move.
 */
export function isGeneralChannel(id: string): boolean {
  const key = id.toLowerCase();
  return key === "" || key === MAIN_THREAD_ID || key === GENERAL_CHANNEL;
}

/**
 * The channel that renders `threadId`, given the shell's thread → channel map.
 *
 * A plain `map[threadId]` is not enough for the General line and never was: the
 * map is seeded with four literal spellings, while the host accepts **any
 * casing** of them and echoes back the one the caller addressed. So a live
 * frame from an API client that posted `MAIN` matched nothing, and its reply
 * and working indicator appeared only once polling recovered the durable
 * history (issue #1743).
 *
 * `null` when the map does not know the thread — never a fall back to whatever
 * the operator has open, which is issue #368's bug.
 */
export function generalAwareChannel(
  map: Readonly<Record<string, string>>,
  threadId: string,
): string | null {
  return map[threadId] ?? (isGeneralChannel(threadId) ? (map[MAIN_THREAD_ID] ?? null) : null);
}

/** One person's reaction on one line. Mirrors `ChatReactionDto` on the host. */
export interface Reaction {
  emoji: string;
  /** Who reacted, as a display label — never a raw user id. */
  by: string;
  /** Whether the reader is the one who reacted. */
  mine: boolean;
}

/** One mention on one line, exactly as resolved by the host. */
export type Mention = ChatMentionDto;

/** One line in the conversation with the company. */
export interface ChatMessage {
  id: string;
  from: "you" | "company" | "system";
  text: string;
  /** Wall-clock the line was added, for timestamps and grouping. */
  at: number;
  /**
   * The reply's originating channel (e.g. "operator"). Threads the company
   * side by sender: a distinct channel reads as its own agent in the chat.
   *
   * **Not a provenance signal.** The offline echo brain names its own outbound
   * channel `operator` too, so this cannot tell a person's message from a
   * canned reply — read {@link byPerson} for that.
   */
  channel?: string;
  /**
   * A person typed this line, rather than the runtime producing it (issue
   * #1734). Read straight off the host; never inferred here.
   *
   * Only set by {@link fromHistory}, because it is the only path a message
   * somebody *else* wrote arrives on — every locally built company line is an
   * agent reply, from this console's own POST or from an `AgentReplyEvent`.
   *
   * `undefined` means the host could not say, and nothing downstream may turn
   * that into a claim in either direction.
   */
  byPerson?: boolean;
  /**
   * The message this one replies to. A line with a parent is a thread reply:
   * it stays out of the channel timeline and renders inside the thread panel.
   *
   * Always another line's `id`, so it moves with {@link reconcileIds} when an
   * optimistic id is replaced by the durable one the host assigned.
   */
  parentId?: string;
  /**
   * Who reacted to this line with what — one row per person per emoji, not a
   * count (issue #364).
   *
   * A count could not say who reacted, and could not tell the reader whether
   * one of the reactions was their own, which is what makes a chip a toggle.
   * The renderer groups these into chips; nothing groups a count back into
   * people. Absent until someone reacts.
   */
  reactions?: Reaction[];
  /**
   * The scrubbed processing steps behind a company reply (tool calls, thinking,
   * surfaced failures), rendered as a timeline above the bubble. Absent/empty
   * on your own messages and on tool-less replies.
   */
  steps?: TurnStep[];
  /**
   * The board card this line is about (issue #246): one the turn opened, or one
   * created from this message by "Add to board". Renders as a chip linking to
   * `#/tasks/<id>`.
   */
  taskId?: string;
  /**
   * Files attached to this line (issue #1682), each a reference into the
   * company workspace. Set on your own message from the composer's pending
   * attachment (optimistic), and rehydrated from `chat/history` on reload so a
   * bubble carries the same chips whichever way it reached the transcript.
   * Absent on a line with no attachment.
   */
  attachments?: AttachmentDto[];
  /**
   * Who this line names (`@engineer`, `@Jane Doe`, `@everyone`), as the host
   * resolved them — spans plus a label, never a target id.
   *
   * Carried rather than re-parsed from `text`, so a chip is drawn only where
   * somebody was actually notified. Absent when the line names nobody, and on
   * a host that predates the field.
   */
  mentions?: Mention[];
}

/**
 * How long a card title derived from a chat message may be before it is
 * elided. Long enough to keep a normal one-line ask intact, short enough that a
 * pasted paragraph does not become a board card nobody can scan.
 */
const TITLE_CAP = 80;

/**
 * A board-card title derived from a chat message (issue #246).
 *
 * Takes the first non-blank line — a multi-line ask reads as "headline, then
 * detail", and the detail belongs in the card's note, which is where the full
 * text goes. Returns an empty string for a message with nothing in it, so the
 * caller can refuse rather than opening a blank card.
 *
 * Elides on **code points**, not UTF-16 units, so a title cut mid-emoji cannot
 * produce a lone surrogate; the cap includes the ellipsis rather than
 * overshooting by one.
 */
export function titleFromMessage(text: string): string {
  const line = text
    .split("\n")
    .map((l) => l.trim())
    .find((l) => l.length > 0);
  if (!line) return "";
  const points = Array.from(line);
  return points.length <= TITLE_CAP
    ? line
    : `${points.slice(0, TITLE_CAP - 1).join("")}…`;
}

let seq = 0;
const nextId = () => `m${seq++}`;

/**
 * The identity a live-streamed company reply must be built with (issue #483).
 *
 * A reply arriving over the stream and the same reply coming back from
 * `chat/history` are one message, and the console has to be able to tell. Both
 * sides carry the host's `StoredEvent` sequence — the stream frame as `seq`,
 * the history entry as `id` — so stamping the live line with it makes the two
 * resolve to the same console id, and `hydrateChannel`'s id dedupe recognises
 * the line it already has.
 *
 * Without this the live line was born under an ephemeral `m<counter>` id that
 * hydration could never match, so a reply that arrived while its channel was
 * closed was rendered twice on opening it.
 *
 * This exists as its own function so the identity decision has somewhere a test
 * can reach. Inlined at the two call sites, nothing could assert it was still
 * being made.
 */
export function liveReplyIdentity(event: { seq: number }): { messageId: string } {
  return { messageId: String(event.seq) };
}

/**
 * How the console labels each board column in a marker.
 *
 * **The one label copy that survives**, and it is deliberate. Everywhere else
 * the console now reads labels off the `tasks` ledger
 * (`lib/board-columns.ts`), which the host builds from its single column table.
 * A marker cannot: it is rendered synchronously from a **thin** SSE frame
 * carrying a raw column id, in a transcript that may have no ledger read behind
 * it at all, and awaiting one would leave a marker line blank or flickering
 * mid-conversation.
 *
 * What makes that acceptable is the blast radius. Drift here can only *reword*
 * one marker; it cannot lose a card, refuse a write, or leave a column
 * unrendered, which is what the board's own copy could do and why that one is
 * gone. It stays in step with the host's `dispatch_marker_text`
 * (`src/server/chat_history.rs`), and unit tests on both sides pin the same
 * literals.
 */
const COLUMN_LABELS: Record<string, string> = {
  // Both vocabularies (issue #1512): a journalled event names the stage it
  // wrote, and anything reading a card off the API names the phase. This line
  // is rendered from either, so it has to label either.
  pending: "Pending",
  working: "Working",
  todo: "To-do",
  planning: "Planning",
  in_progress: "In progress",
  paused: "Paused",
  in_review: "In review",
  done: "Done",
};

/**
 * The channel line a settled dispatch leaves behind (issue #377) —
 * `finished → In review`.
 *
 * The console needs its own copy because the live SSE frame is **thin**: it
 * carries the raw column id, not prose, exactly as `task_card_changed` and
 * `approval_parked` do. The host holds the same sentence for the rehydrated
 * half (`dispatch_marker_text`), and unit tests on both sides pin the identical
 * literals. See `COLUMN_LABELS` above for why this copy is the one exception
 * to the ledger-driven rule.
 *
 * Drift between the two can only *reword* a marker across a reload; it can
 * never double one, because the dedupe is on identity (`h<seq>`) and not on
 * content. That was #483's lesson and it is what makes two spellings a
 * tolerable cost rather than a returning bug.
 *
 * "finished" means the run **stopped**, not that it succeeded — a cancelled or
 * failed dispatch says `finished → To-do`, a parked one `finished → Paused`.
 * The misleading case this exists for is precisely the run that stopped without
 * finishing the work. An unrecognised column id passes through verbatim rather
 * than rendering blank.
 */
export function dispatchMarkerText(column: string): string {
  return `finished → ${COLUMN_LABELS[column] ?? column}`;
}

/**
 * Build a stamped message. `at` is injected so callers stay pure/testable.
 *
 * `messageId` is the host's own id for the line, when the caller already has
 * one — a reply that came back from the chat POST does. Passing it means the
 * bubble is born durable and can be replied to or reacted on immediately,
 * rather than waiting for a reconciliation it does not need.
 */
export function makeMessage(
  from: ChatMessage["from"],
  text: string,
  opts: {
    channel?: string;
    at?: number;
    parentId?: string;
    steps?: TurnStep[];
    taskId?: string;
    messageId?: string;
    attachments?: AttachmentDto[];
    /** Mention spans the host resolved against this message, for chip rendering. */
    mentions?: Mention[];
  } = {},
): ChatMessage {
  return {
    id: opts.messageId ? hostMessageId(opts.messageId) : nextId(),
    from,
    text,
    at: opts.at ?? Date.now(),
    channel: opts.channel,
    parentId: opts.parentId,
    steps: opts.steps,
    taskId: opts.taskId,
    // Issue #1682: an empty list is dropped to `undefined` so a line with no
    // attachment stays exactly the shape it was before the field existed.
    attachments: opts.attachments?.length ? opts.attachments : undefined,
    mentions: opts.mentions?.length ? opts.mentions : undefined,
  };
}

/** The fields {@link dispatchMarkerPlacement} reads off a `desk_task_completed` frame. */
export interface DispatchTerminalFrame {
  taskId: string;
  column: string;
  /** The channel the card was raised in; absent for a board-created card. */
  chatId?: string;
  /** The host's `StoredEvent` sequence — the marker's durable identity. */
  seq: number;
  atMillis: number;
}

/** Where a settled dispatch's marker goes, and the line to put there. */
export interface DispatchMarkerPlacement {
  /** The chat thread the card was raised in. */
  threadId: string;
  /** The channel rendering that thread, or `null` when this company has none. */
  channelId: string | null;
  message: ChatMessage;
}

/**
 * Decide where a settled dispatch's marker belongs (issue #377), or that it
 * belongs nowhere.
 *
 * A named function rather than three lines inside the shell's injector, for the
 * reason {@link liveReplyIdentity} is one: every rule below is a decision that
 * silently stops being made if it is inlined, and each of them has a specific
 * bug on the other side of it.
 *
 * - **No `chatId` → `null`.** Nothing raised that card from a conversation (it
 *   was opened on the board, or by a scheduler), so no channel is the right
 *   one. The host already declines to file such a card into any desk's history;
 *   this is the live half of the same rule, and the two must agree or a marker
 *   would appear live and vanish on reload.
 * - **An empty `chatId` is the General thread, not an absent one.** The host
 *   folds `""` into General (`is_general_chat` treats an empty id, `"main"` and
 *   `"General"` as one conversation), and the chat route takes `chat` straight
 *   off the request body without normalising it — so a client posting
 *   `chat: ""` stores `origin_chat_id: Some("")` and the projection emits
 *   `chatId: ""`. Treating that as absent would drop the live marker while
 *   `chat/history` still served the rehydrated twin: the marker would appear
 *   only after a reload, which is the live-vs-history split the whole
 *   identity-dedupe exists to prevent. Absent means `undefined`, and only
 *   `undefined`. (The REST create path already normalises a blank field away
 *   for the same reason — issue #246.)
 * - **`channelId: null` when the thread matches no channel** — never a fall
 *   back to whatever channel the operator has open. The shell's `noteInChannel`
 *   does fall back, deliberately, because an approval decision has to be seen;
 *   a marker does not, and falling back would file one conversation's settle
 *   into another. That is issue #368's bug, and this is the shape that makes
 *   re-introducing it impossible rather than merely discouraged.
 * - **The line is born under the host's id** (`h<seq>`), which is exactly what
 *   `chat/history` mints for the same event — so {@link fromHistory}'s twin
 *   dedupes against it on the next reload. Identity, not content: #483 was a
 *   content check hydration could never satisfy.
 */
export function dispatchMarkerPlacement(
  event: DispatchTerminalFrame,
  chatChannelByThread: Record<string, string>,
): DispatchMarkerPlacement | null {
  if (event.chatId === undefined) return null;
  // `""` is the General thread spelled empty, not a missing origin — see above.
  const threadId = event.chatId === "" ? MAIN_THREAD_ID : event.chatId;
  return {
    threadId,
    // Resolved the same way every other live frame is (issue #1743): the map
    // carries four literal General spellings, while the host accepts any casing
    // and echoes back the one the caller addressed. A bare index dropped the
    // marker for `MAIN` or `GENERAL`, so a dispatch settled with nothing to
    // show for it until a reload.
    channelId: generalAwareChannel(chatChannelByThread, threadId),
    message: makeMessage("system", dispatchMarkerText(event.column), {
      taskId: event.taskId,
      messageId: String(event.seq),
      at: event.atMillis,
    }),
  };
}

/**
 * Maps a desk's persisted transcript (`GET .../chat/history`, issue #65) to
 * the console's chat lines, preserving `mine`/author/text and ordering — the
 * backend already returns messages oldest-first. `id`s are namespaced with an
 * `h` prefix so a rehydrated line can never collide with one built locally by
 * {@link makeMessage} (`m<seq>`).
 */
export function fromHistory(entries: ChatHistoryMessageDto[]): ChatMessage[] {
  return entries.map((entry) => {
    // A host-authored system line (issue #377's dispatch marker) is neither
    // yours nor the company's voice: it renders as a centred pill, not a
    // bubble. Read off the author the host projected — `mine` is false for it,
    // so without this check a rehydrated marker came back as a company message
    // and a settle read like something an agent had said.
    const from: ChatMessage["from"] =
      entry.author === "system" ? "system" : entry.mine ? "you" : "company";
    return {
      id: hostMessageId(entry.id),
      from,
      text: entry.text,
      at: entry.atMillis,
      // Straight through, never derived: see the field's own note, and
      // `MessageView::by_person` for why the host is the only layer that knows.
      byPerson: entry.byPerson,
      // The host names the parent by its own id, which lives in the same
      // namespace as `entry.id` — so it takes the same prefix, or the reply
      // would point at a line no console id matches (issue #364).
      parentId: entry.parentId ? hostMessageId(entry.parentId) : undefined,
      // Reactions come through whoever the host said reacted; nothing is
      // inferred here, `mine` included.
      reactions: entry.reactions?.length ? entry.reactions : undefined,
      // Same rule as reactions: the host says who was mentioned, and nothing
      // here infers one from the text.
      mentions: entry.mentions?.length ? entry.mentions : undefined,
      // A sent message never carries a channel; only attribute one when the
      // line came from someone/something else, mirroring `ChatPane.send`.
      channel: from === "company" ? entry.channel : undefined,
      // Rehydrate the persisted tool-call timeline so it survives a thread
      // switch / reload — the render already draws `m.steps` (Conversation.tsx).
      steps: from === "company" ? entry.steps : undefined,
      // Rehydrate the "card opened" chip (issue #246) for the same reason as
      // the timeline above: a chip that lives only on the live POST response
      // vanishes on the first thread switch.
      //
      // System rows carry it too (issue #377): a dispatch marker's whole
      // usefulness is the link to the card that settled, and dropping the id
      // here would leave a rehydrated marker as a sentence with nowhere to go.
      // Only your own lines never have one — you did not open a card by
      // speaking.
      taskId: from === "you" ? undefined : entry.taskId,
      // Rehydrate the operator's attachments (issue #1682) so a bubble carries
      // the same chips on reload it showed live. Empty drops to `undefined`,
      // keeping the pre-#1682 line shape.
      attachments: entry.attachments?.length ? entry.attachments : undefined,
    };
  });
}

/**
 * The console id for a message the host has journaled (issue #364).
 *
 * Two id namespaces meet in a transcript, and keeping them apart is the whole
 * job of this prefix: `m<n>` is a browser-minted counter that means nothing
 * outside this tab, and `h<seq>` is the host's own sequence position, which any
 * reader — a reload, a second operator — resolves to the same message. The `h`
 * is what lets {@link isHostMessageId} tell "saved" from "not saved yet"
 * without asking the server.
 */
export function hostMessageId(seq: string): string {
  return `h${seq}`;
}

/** Whether an id names a message the host has journaled. */
export function isHostMessageId(id: string | undefined): boolean {
  return !!id && id.startsWith("h");
}

/**
 * The host-side id an `h`-prefixed console id names, or `null` for a local one.
 *
 * The inverse of {@link hostMessageId}, used when a durable id has to go back
 * over the wire — a thread reply's parent, a reaction's target.
 */
export function toHostMessageId(id: string | undefined): string | null {
  return isHostMessageId(id) ? id!.slice(1) : null;
}

/**
 * Replace an optimistic message id with the durable one the host assigned, and
 * re-point anything that was already replying to it (issue #364).
 *
 * The subtle half is the re-parenting, and it is why this is a named function
 * with its own tests rather than three lines inside a `setState`. A send is
 * rendered before the POST resolves, so a fast operator can open a thread on
 * that bubble and reply to it while its id is still the local counter. Swapping
 * only the parent's own id would leave those replies pointing at an id that no
 * longer exists — they would silently drop out of the thread and reappear in
 * the channel, which reads as the console losing them.
 *
 * Pure and total: unknown ids pass through, and a list with nothing to change
 * is returned as-is so React sees no new array.
 */
export function reconcileIds(
  messages: ChatMessage[],
  localId: string,
  hostSeq: string,
): ChatMessage[] {
  const nextId = hostMessageId(hostSeq);
  if (nextId === localId) return messages;
  let changed = false;
  const next = messages.map((m) => {
    const isTarget = m.id === localId;
    const isChild = m.parentId === localId;
    if (!isTarget && !isChild) return m;
    changed = true;
    return {
      ...m,
      ...(isTarget ? { id: nextId } : {}),
      ...(isChild ? { parentId: nextId } : {}),
    };
  });
  return changed ? next : messages;
}

/**
 * Forget the board card `taskId` on every line that carries it (issue #984).
 *
 * The dismissal half of "Add to board": #442 justified opening cards from chat
 * on the grounds that *"a spurious card can be dismissed in one click"*, and
 * the chat surfaces offered no such click — the chip was a bare link to the
 * card's detail screen. Deleting the card on the host is only half of it; this
 * is what stops the console still drawing a chip for a card that is gone.
 *
 * Keyed on the **card**, not on the message the operator clicked, and that is
 * the reason this is a named function rather than two lines inside a
 * `setState`. One card can be named by more than one line — a turn journals the
 * id onto its reply, and "Add to board" writes it onto the operator's own
 * message — so clearing only the clicked bubble would leave the other chips
 * pointing at a card the host no longer has, i.e. a link to a 404. A dismissal
 * that leaves a stale chip on screen reads as the delete having failed.
 *
 * Pure and total: an unknown id passes through, and a list with nothing to
 * change is returned as-is so React sees no new array.
 *
 * # This is the client half only
 *
 * It clears the chip from state that is never serialised, so on its own it
 * survives a thread switch and **not** a reload: the console rehydrates from
 * `GET …/chat/history` (see {@link fromHistory}) and merges by message id, so an
 * empty transcript takes every row back. The host is what stops the chip
 * returning — its history projection blanks `task_id` for a card the board no
 * longer has, whoever deleted it (`server::chat_history::drop_dead_cards`,
 * issue #984). Neither half is sufficient alone.
 */
export function clearTaskCard(messages: ChatMessage[], taskId: string): ChatMessage[] {
  let changed = false;
  const next = messages.map((m) => {
    if (m.taskId !== taskId) return m;
    changed = true;
    // Drop the key rather than setting it to `undefined`: `taskId` is an
    // optional field and every render site tests it for truthiness, but the two
    // are not interchangeable to `Object.keys`, a spread, or a deep-equality
    // assertion, and an absent key is what "this line has no card" means
    // everywhere else in this module.
    //
    // NOT because these rows are persisted — they are not. `transcripts` is
    // React state and is never serialised. The rows that persist are the
    // host's, and they are why this helper is only half the dismissal: see the
    // note on the function above.
    const { taskId: _dropped, ...rest } = m;
    return rest;
  });
  return changed ? next : messages;
}

/** Stable content fields shared by an optimistic row and its history echo. */
function messageFingerprint(message: ChatMessage): string {
  return JSON.stringify([message.from, message.text, message.parentId ?? null]);
}

/** The host event sequence encoded by a durable console id, when available. */
function messageSequence(message: ChatMessage): number | null {
  if (!isHostMessageId(message.id)) return null;
  const sequence = Number(toHostMessageId(message.id));
  return Number.isSafeInteger(sequence) ? sequence : null;
}

/**
 * Fold one `chat/history` response into a live transcript (issue #1690).
 *
 * `chat/history` is the host's authoritative, **oldest-first** record for a
 * thread; a live transcript is that record plus whatever has not been
 * journaled yet — the operator's own optimistic bubbles, minted with
 * browser-local `m<seq>` ids the host does not know. Folding the response in
 * is therefore neither prepend nor append but *reconstruction*, and the two
 * rules below are what make that safe:
 *
 * 1. **Persisted rows take the history's own order.** A reply the live SSE
 *    path missed lands after the message it follows (a plain append/prepend
 *    rule could put it *before* the transcript), and a gap the SSE path
 *    dropped is filled in its correct position rather than tacked on after
 *    the tail — `[1, 3]` + history `[1, 2, 3]` must merge to `[1, 2, 3]`,
 *    never `[1, 3, 2]`.
 * 2. **Rows the host has not persisted yet stay in their live order.** They
 *    are inserted at the boundary implied by their durable neighbours, while
 *    optimistic sends with no durable sequence remain at the tail.
 *
 * Rows the history names that are already on screen are kept as the caller's
 * own objects (reactions and other local decoration survive the re-fetch)
 * rather than replaced by the freshly-projected copy. When nothing differs the
 * input array is returned unchanged, so a caller can bail out of a state write
 * and React can skip a re-render.
 */
export function mergeHistoryInOrder(
  existing: ChatMessage[],
  hydrated: ChatMessage[],
): ChatMessage[] {
  const historyIds = new Set(hydrated.map((m) => m.id));
  const existingById = new Map(existing.map((m) => [m.id, m]));
  const durableEchoes = new Map<string, ChatMessage[]>();
  for (const message of hydrated) {
    const matches = durableEchoes.get(messageFingerprint(message)) ?? [];
    matches.push(message);
    durableEchoes.set(messageFingerprint(message), matches);
  }

  const persisted = hydrated.map((m) => existingById.get(m.id) ?? m);
  const consumedEchoes = new Set<ChatMessage>();
  const liveRows = existing.filter((m) => !historyIds.has(m.id));
  const liveDurable = liveRows.filter((m) => isHostMessageId(m.id));
  const hydratedSequences = hydrated.map(messageSequence);
  const firstSequence = hydratedSequences.find((sequence) => sequence !== null) ?? null;
  const lastSequence = [...hydratedSequences]
    .reverse()
    .find((sequence) => sequence !== null) ?? null;
  const snapshotAt = hydrated.length ? hydrated[hydrated.length - 1].at : null;

  const optimistic = liveRows.filter((message) => {
    if (isHostMessageId(message.id)) return false;
    const matches = durableEchoes.get(messageFingerprint(message));
    const echo = matches?.find((candidate) => {
      if (consumedEchoes.has(candidate)) return false;
      // A durable row already present in the live transcript is an older
      // identical send, even when it is the newest (or only) row in the page.
      // Only an id that was not on screen when this fold began can reconcile
      // this local bubble; otherwise a send made after the snapshot would be
      // consumed by the page-boundary row it duplicated.
      if (existingById.has(candidate.id)) return false;
      // A matching row before the snapshot may be an older identical message,
      // not this send. A row at/after the snapshot is fresh evidence; for a
      // single-row response there is no older page boundary to consult, but
      // the id check above still protects that boundary when it was live.
      return snapshotAt === null || hydrated.length === 1 || candidate.at >= snapshotAt;
    });
    if (!echo) return true;
    consumedEchoes.add(echo);
    return false;
  });

  const outsidePage = liveDurable.filter((message) => {
    const sequence = messageSequence(message);
    if (sequence !== null && firstSequence !== null && lastSequence !== null) {
      // A durable row absent from the page may be a gap or a live tail. Keep
      // every such row and let sequence-aware insertion restore its position.
      return true;
    }
    if (!hydrated.length) return true;
    return message.at <= hydrated[0].at || message.at >= hydrated[hydrated.length - 1].at;
  });

  // Start with the authoritative page, then insert rows that were already
  // live. Numeric host sequences are the ordering authority; timestamps are a
  // compatibility fallback for legacy/non-numeric ids. Finally insert local
  // rows without a durable position at the boundary implied by their live
  // neighbours, rather than moving every optimistic send to the tail.
  const merged = [...persisted];
  for (const message of outsidePage) {
    const sequence = messageSequence(message);
    let index = -1;
    if (sequence !== null) {
      index = merged.findIndex((candidate) => {
        const candidateSequence = messageSequence(candidate);
        return candidateSequence !== null && candidateSequence > sequence;
      });
    } else {
      index = merged.findIndex((candidate) => candidate.at > message.at);
    }
    merged.splice(index < 0 ? merged.length : index, 0, message);
  }

  for (const message of optimistic) {
    const liveIndex = liveRows.indexOf(message);
    // A local row's position is defined by the nearest durable live row on
    // either side of it. The preceding live row may itself be optimistic and
    // already inserted; using it as the lower bound preserves the order of a
    // burst of local sends around one durable SSE row.
    const previousLive = liveRows
      .slice(0, liveIndex)
      .reverse()
      .find((candidate) => merged.includes(candidate));
    const nextLive = liveRows.slice(liveIndex + 1).find((candidate) => merged.includes(candidate));
    if (previousLive) {
      merged.splice(merged.indexOf(previousLive) + 1, 0, message);
    } else if (nextLive) {
      merged.splice(merged.indexOf(nextLive), 0, message);
    } else {
      merged.push(message);
    }
  }

  return merged.length === existing.length && merged.every((m, i) => m === existing[i])
    ? existing
    : merged;
}
