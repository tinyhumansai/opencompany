// The composer's half of @-mentions: what the caret is currently typing, what
// to insert when a row is picked, and which mentions a draft still carries.
//
// Pure by design — the repo keeps chat logic in `model.ts`/`lib` with a unit
// test rather than inline in a component, and every rule below is one that is
// much easier to get wrong in a keydown handler than in a function.
//
// The server has a twin of this in `src/runtime/mentions.rs`. It is the
// authority: whatever the picker resolves here is re-validated there against
// the live roster, and a target that no longer exists is demoted rather than
// trusted. This side exists to make the *typing* good, not to be believed.

/** How many mentions one message may carry as pings. Mirrors `MENTION_CAP`. */
export const MENTION_CAP = 50;

/** A regular expression that matches nothing, ever. */
const NEVER_MATCH = /(?!)/g;

/** Longest `@query` the picker will keep open before giving up. */
const MAX_QUERY = 32;

import type { ChatMentionInput, MentionTarget } from "@/api/types";

// The composer produces a wire value, so the shape it builds is the API's, not
// a parallel one that would have to be kept in step with it.
export type { MentionTarget };

/** One resolved mention, as the composer sends it. */
export type Mention = ChatMentionInput;

/** One row the picker can offer. */
export interface Mentionable {
  target: MentionTarget;
  /** What to render, and what gets inserted after the `@`. */
  label: string;
  /** The person's collaboration-facing avatar reference, when chosen. */
  avatar?: string;
  /** Every spelling that reaches this row, lowercase. */
  aliases: string[];
  /** A line of context under the label — a job title, a slug, a member count. */
  hint?: string;
  /** Whether this row is on the channel being composed in. Ranks first. */
  inChannel?: boolean;
  /**
   * The teammates a mention of this desk expands to. Set on desk rows only,
   * so the outside-channel warning can judge a desk mention by its blast
   * radius rather than skipping it.
   */
  memberIds?: string[];
}

/**
 * Whether the `@` at `i` opens a mention.
 *
 * The condition that keeps `jane@acme.com` from opening a picker mid-address:
 * an `@` counts only at the start or after whitespace or an opening bracket,
 * and never immediately after a `<`. Anything else — `/docs/@eng`, `$@eng` —
 * is part of some other token, and a picker there would resolve a mention the
 * host's fallback extraction (`opens_mention`) would refuse.
 */
function opensMention(text: string, i: number): boolean {
  if (text[i] !== "@") return false;
  const before = text[i - 1];
  // Start of text always opens.
  if (before === undefined) return true;
  // Never inside an existing `<@id>` token. The bracket check below would
  // already refuse it, but the rule is documented, so it is stated.
  if (before === "<") return false;
  // Mirrors `opens_mention` in `src/runtime/mentions.rs` exactly: only ASCII
  // whitespace or an opening bracket opens. JavaScript `\s` is wider and would
  // open a picker after a pasted non-breaking space the host then drops.
  return /[ \t\n\r([{]/.test(before);
}

/**
 * The `@query` the caret is currently inside, or `null` when it is not in one.
 *
 * Scans backwards from the caret to the nearest mention-opening `@`, giving up
 * at a newline, a second `@`, or {@link MAX_QUERY} characters — so a stray `@`
 * earlier in a long paragraph cannot hold the picker open while somebody types
 * an unrelated sentence.
 *
 * The query may contain spaces, because a person's label often does
 * (`@Jane Doe`). That is what makes the multi-word case reachable at all, and
 * why the give-up conditions above have to be real rather than "stop at a
 * space".
 */
export function activeMentionQuery(
  text: string,
  caret: number,
  knownAliases?: ReadonlySet<string>,
): { start: number; query: string } | null {
  for (let i = caret - 1; i >= 0 && caret - i <= MAX_QUERY; i--) {
    const ch = text[i];
    if (ch === "\n") return null;
    if (ch !== "@") continue;
    if (!opensMention(text, i)) return null;
    const query = text.slice(i + 1, caret);
    // A finished name followed by a space closes the query.
    //
    // Without this the picker reopens the instant you pick somebody: inserting
    // `@engineer ` leaves the caret after a trailing space, the scan above
    // still finds the `@`, and the query becomes `"engineer "` — which matches
    // and re-renders the list over the message you are now trying to write.
    //
    // Gated on an EXACT alias rather than on any trailing space, because a
    // space is also how a two-word name is typed: `@Jane ` must stay open on
    // the way to `@Jane Doe`. `block/buzz` draws the line in the same place,
    // and for the same reason — a longer name sharing a prefix must not hold
    // the query open once a shorter one is complete.
    if (knownAliases && /\s$/.test(query)) {
      if (knownAliases.has(query.trim().toLowerCase())) return null;
    }
    return { start: i, query };
  }
  return null;
}

/**
 * Every spelling in `entries`, for {@link activeMentionQuery}'s close rule.
 *
 * Built by the caller once per directory rather than per keystroke.
 */
export function aliasSet(entries: readonly Mentionable[]): Set<string> {
  const out = new Set<string>();
  for (const entry of entries) {
    out.add(entry.label.toLowerCase());
    for (const alias of entry.aliases) out.add(alias);
  }
  return out;
}

/**
 * Blanks fenced and inline code spans, preserving every offset.
 *
 * Each masked byte becomes a space, so the result is the same length as the
 * input and an index computed against it is valid against the original.
 * Stripping instead would shift every later match.
 */
export function stripCodeRegions(text: string): string {
  const out = text.split("");
  const blank = (from: number, to: number) => {
    for (let i = from; i < Math.min(to, out.length); i++) {
      if (out[i] !== "\n") out[i] = " ";
    }
  };

  // Fenced blocks first, so a backtick inside one is never read as a span.
  // CommonMark allows a fence up to three spaces of indentation, so both the
  // opening and closing delimiters accept it — a fence indented by three spaces
  // renders as code, and an `@` inside it must not open the picker either. A
  // closing fence may be followed only by spaces or tabs (or a CR in a CRLF
  // line ending), never by text: `````not-a-close``` is still inside the block
  // in CommonMark, so it must not close the mask early and unmask a later `@`
  // the renderer still shows as code.
  const fence =
    /^ {0,3}([`~]{3,})[^\n]*\n?([\s\S]*?)(?:^ {0,3}\1[`~]*[ \t\r]*$|$)/gm;
  for (const m of text.matchAll(fence)) {
    if (m.index !== undefined) blank(m.index, m.index + m[0].length);
  }

  const masked = out.join("");
  // Inline spans, closed by a maximal backtick run of exactly equal length —
  // CommonMark, like the Rust scanner, only lets a *whole* run close a span.
  // Without the boundary guards, `@engineer`` (one opener, two trailing)
  // would close on the first of the trailing pair, mask a mention the
  // renderer still shows as visible text, and drop it from resolution.
  const span = /(?<![`])(`+)(?!`)(?:(?!\1)[\s\S])*?(?<![`])\1(?!`)/g;
  for (const m of masked.matchAll(span)) {
    if (m.index !== undefined) blank(m.index, m.index + m[0].length);
  }
  return out.join("");
}

/**
 * Orders the picker's rows for `query`.
 *
 * Two levels, following the shape `block/buzz` settled on: a group rank first,
 * so people you are actually talking to come before the long tail, then a
 * match score, then the original order. Sorting by score alone buries a channel
 * member under a better-spelled stranger.
 */
export function rankMentionables(
  entries: Mentionable[],
  query: string,
): Mentionable[] {
  const q = query.trim().toLowerCase();
  const groupRank = (e: Mentionable): number => {
    if (e.inChannel) return 0;
    if (e.target.kind === "everyone") return 1;
    if (e.target.kind === "user") return 2;
    if (e.target.kind === "desk") return 3;
    return 4;
  };
  const score = (e: Mentionable): number => {
    if (!q) return 0;
    let best = Number.MAX_SAFE_INTEGER;
    for (const alias of e.aliases) {
      if (alias === q) best = Math.min(best, 0);
      else if (alias.startsWith(q)) best = Math.min(best, 1);
      else if (alias.split(/[\s\-_.]+/).some((w) => w === q))
        best = Math.min(best, 2);
      else if (alias.split(/[\s\-_.]+/).some((w) => w.startsWith(q)))
        best = Math.min(best, 3);
      else if (alias.includes(q)) best = Math.min(best, 4);
    }
    return best;
  };

  return entries
    .map((entry, index) => ({ entry, index, s: score(entry) }))
    .filter((row) => row.s !== Number.MAX_SAFE_INTEGER)
    .sort(
      (a, b) =>
        // An exact match outranks the group preference, and only an exact
        // match does. Otherwise typing a teammate's whole name still hands you
        // the desk that merely starts with it — `@engineer` offering
        // `engineering` first — which is precisely the case where the person
        // has already told you exactly who they mean.
        (a.s === 0 ? 0 : 1) - (b.s === 0 ? 0 : 1) ||
        groupRank(a.entry) - groupRank(b.entry) ||
        a.s - b.s ||
        a.index - b.index,
    )
    .map((row) => row.entry);
}

/**
 * The mentions a replacement `[start, end)` destroys, dropped from `mentions`.
 *
 * A picker selection replaces the token under the caret, and any mention whose
 * recorded span the range touches no longer exists in the draft — its text was
 * overwritten. It has to be dropped **before** the new mention is reconciled,
 * or the replaced identity survives as a span that text-only reconciliation
 * re-anchors onto an unrelated same-text occurrence: in `@Sam then @Sam`,
 * replacing the picked first `@Sam` with `@engineer` would otherwise move
 * Sam's selected identity onto the second, hand-typed `@Sam` and notify the
 * wrong person.
 *
 * Non-overlapping mentions are kept; `reconcileMentions` shifts them past the
 * edit as usual.
 */
export function mentionsOutsideRange(
  mentions: readonly Mention[],
  range: { start: number; end: number },
): Mention[] {
  return mentions.filter((m) => {
    const end = m.offset + m.text.length;
    return !(m.offset < range.end && end > range.start);
  });
}

/**
 * Replaces the active `@query` with `entry`, and says where the caret lands.
 *
 * A trailing space is appended so the next word does not extend the mention —
 * without it, typing straight on after picking would grow the span and the
 * reconcile below would drop it.
 */
export function insertMention(
  draft: string,
  range: { start: number; end: number },
  entry: Mentionable,
): { text: string; caret: number; mention: Mention } {
  const span = `@${mentionableText(entry)}`;
  const text = `${draft.slice(0, range.start)}${span} ${draft.slice(range.end)}`;
  return {
    text,
    caret: range.start + span.length + 1,
    mention: { target: entry.target, text: span, offset: range.start },
  };
}

/**
 * The `@`-spelling of a row's mention the host will actually resolve.
 *
 * The host's scanner only opens a mention when the character right after `@`
 * is word-like (alphanumeric or `_`, or `#` for the `@#desk` spelling), so a
 * person whose display name starts with an emoji or punctuation — `👩‍💻 Ada` —
 * cannot be picked by their label: `@👩‍💻 Ada` fails server revalidation and
 * the mention is dropped, leaving a chip-less literal that pings nobody. The
 * row's own `aliases` carry a typable fallback (the host-minted slug), so the
 * picker inserts that instead, keeping the picked mention real.
 *
 * The `#` spelling is desk-only: the host's fallback extraction narrows
 * `@#…` to desk targets, so accepting a `#`-prefixed label for a user or
 * agent row would insert a visually desk-shaped mention that revalidation
 * still lets through (it strips the hash without checking the target kind).
 * A `#`-led label only wins here on a desk row, where `@#name` is exactly
 * the spelling the host resolves; every other kind falls back to its slug.
 *
 * The spelling must also survive the message renderer intact. A label with
 * inline Markdown delimiters — `Ada *Ops*`, ``Ada `Ops` ``, `Ada [Ops]` —
 * is inserted verbatim as `@Ada *Ops*`, and the host routes it fine, but
 * react-markdown splits the raw span into text and formatting nodes, so
 * `chipMentions` can never match the mention's full text against a single
 * rendered node and draws no chip. Like the emoji case, the row's typable
 * fallback wins instead. Intraword `_` stays allowed: CommonMark renders
 * `@Jane_Smith` as one literal text node, so the chip matches.
 */
function mentionableText(entry: Mentionable): string {
  // Characters that open an inline-formatting construct anywhere in a
  // span — backtick, `*`, `~`, `[`, `]`, `\` — render text differently
  // from its raw form, so such a spelling must not be picked verbatim.
  const plain = (s: string) => !/[*~`[\]\\]/u.test(s);
  const opens = (s: string) =>
    plain(s) &&
    (/^[\p{L}\p{N}_]/u.test(s) ||
      (entry.target.kind === "desk" && /^#[\p{L}\p{N}_]/u.test(s)));
  if (opens(entry.label)) return entry.label;
  for (const alias of entry.aliases) {
    if (opens(alias)) return alias;
  }
  return entry.label;
}

/**
 * Drops every mention whose span no longer sits where it was recorded, and
 * shifts the ones that merely moved.
 *
 * This is what makes backspacing through a chip un-mention it: edit the text of
 * `@Jane Doe` and the recorded span stops matching, so the mention goes with
 * it. Without this the composer would keep pinging somebody whose name is no
 * longer in the message.
 *
 * A mention that still exists verbatim but has shifted — because text was
 * inserted before it — is re-anchored rather than dropped, so typing at the
 * start of a draft does not silently unresolve everything after the caret.
 * `editCaret`, when supplied by the textarea change handler, disambiguates a
 * pure literal insertion whose contents duplicate an existing mention.
 *
 * Two mentions with the **same literal** are matched by their recorded order,
 * not greedily by text. When `@Sam @Sam` names two people and the first span
 * is deleted, the remaining text is the second Sam's — but a forward scan sees
 * the first Sam's old span still sitting at offset 0, keeps it, and drops the
 * survivor as overlapping. Processing from the last mention back lets the
 * survivor claim the freed occurrence first, and a displaced mention re-anchors
 * to the free occurrence closest to where it was rather than to the earliest.
 *
 * The reverse scan is a fixed bias: it assumes the *later* duplicate survived,
 * which is exactly right when the first was deleted and exactly wrong when the
 * second was. Those two edits produce identical `(text, mentions)` — `@Sam`
 * with both spans recorded at 0 and 5 — so no text-only rule can serve both,
 * which is why the caller with the pre-edit text hands it over: `previous`
 * lets the edited region be recovered, and a mention whose recorded span the
 * edit touched is the one that was removed or broken. Dropping it before the
 * reverse scan runs keeps its same-text sibling's identity instead of
 * re-anchoring the broken mention onto the survivor's span. The composer
 * passes the previous draft on `onChange`; the pick/send/trim paths omit it
 * and keep the historical behavior.
 */
export function reconcileMentions(
  text: string,
  mentions: Mention[],
  previous?: string,
  editCaret?: number,
): Mention[] {
  const used: Array<[number, number]> = [];
  const out: Mention[] = [];
  const overlaps = (at: number, len: number) =>
    used.some(([s, e]) => at < e && at + len > s);
  const claim = (mention: Mention, at: number) => {
    used.push([at, at + mention.text.length]);
    out.push({ ...mention, offset: at });
  };

  // The edited region in `previous`'s coordinates: the common prefix, then the
  // common suffix. Anything between them is what the edit removed. A mention
  // whose span the edit touched is dropped before the reverse scan, so a
  // broken mention cannot re-anchor onto an unrelated same-text occurrence.
  //
  // The overlap test covers both kinds of edit: a deletion (or replacement)
  // leaves a non-empty `[deletedStart, deletedEnd)` that overlaps the span,
  // while a pure insertion collapses the region to `[p, p)` — empty by length
  // but still meaning "the insertion point `p` sits strictly inside the span",
  // which is exactly the case where an inserted character broke the mention
  // text. So the filter runs unconditionally; guarding on `deletedEnd >
  // deletedStart` would let the insertion case slip through and re-anchor.
  if (previous && previous !== text) {
    let prefix = 0;
    const maxPrefix = Math.min(previous.length, text.length);
    while (prefix < maxPrefix && previous[prefix] === text[prefix]) prefix += 1;
    let suffix = 0;
    const maxSuffix = Math.min(previous.length, text.length) - prefix;
    while (
      suffix < maxSuffix &&
      previous[previous.length - 1 - suffix] === text[text.length - 1 - suffix]
    ) {
      suffix += 1;
    }
    const deletedStart = prefix;
    const deletedEnd = previous.length - suffix;
    // For a literal insertion, a common-prefix scan consumes the unchanged
    // duplicate text and reports the insertion at the end. The textarea caret
    // identifies the real boundary, so use it to shift mentions at/after that
    // point and leave the inserted occurrence unclaimed by the old identity.
    if (
      editCaret !== undefined &&
      deletedStart === deletedEnd &&
      text.length > previous.length
    ) {
      const insertedLength = text.length - previous.length;
      mentions = mentions.map((m) =>
        m.offset >= editCaret - insertedLength
          ? { ...m, offset: m.offset + insertedLength }
          : m,
      );
    }
    // The edit caret is in the post-edit string. For an insertion at the end
    // of an old span, the old mention is before the caret and should remain at
    // its original offset; for an insertion before or inside it, the mention
    // has shifted and the ordinary re-anchor scan below finds its new span.
    mentions = mentions.filter((m) => {
      const end = m.offset + m.text.length;
      return !(m.offset < deletedEnd && end > deletedStart);
    });
  }

  for (let i = mentions.length - 1; i >= 0; i--) {
    const mention = mentions[i];
    // Prefer the recorded occurrence only when it is not also the start of an
    // occurrence that belongs to a later duplicate. When identical text was
    // inserted at an earlier offset, the old offset is ambiguous; the later
    // mention's recorded position identifies the newly inserted occurrence and
    // lets this one re-anchor to the next occurrence instead.
    const laterDuplicateAtSameOffset = mentions
      .slice(i + 1)
      .some(
        (later) =>
          later.text === mention.text &&
          later.offset === mention.offset &&
          text.slice(later.offset, later.offset + later.text.length) === later.text,
      );
    if (
      !laterDuplicateAtSameOffset &&
      !overlaps(mention.offset, mention.text.length) &&
      text.slice(mention.offset, mention.offset + mention.text.length) === mention.text
    ) {
      claim(mention, mention.offset);
      continue;
    }
    // Re-anchor to the free occurrence closest to where it was. Editing shifts
    // a mention's home by the edit's size, so the nearest same-text span is
    // the one most likely to be it.
    let nearest: number | undefined;
    let from = 0;
    for (;;) {
      const at = text.indexOf(mention.text, from);
      if (at === -1) break;
      if (
        !overlaps(at, mention.text.length) &&
        (nearest === undefined ||
          Math.abs(at - mention.offset) < Math.abs(nearest - mention.offset))
      ) {
        nearest = at;
      }
      from = at + 1;
    }
    if (nearest !== undefined) claim(mention, nearest);
  }
  return out.sort((a, b) => a.offset - b.offset);
}

/**
 * Re-anchor mentions after a toolbar `wrap` inserts `mark` at both ends of the
 * selection `[start, end)`.
 *
 * This is a two-ended insertion, so it does not behave like the single-edit
 * case {@link reconcileMentions} is built for: there the whole edited region is
 * treated as deleted, which would drop a mention merely *enclosed* by the wrap.
 * Formatting a mention leaves its literal intact (``**@Sam**`` still reads
 * `@Sam`), so an enclosed mention keeps its target and shifts by the leading
 * mark. Only an insertion point strictly inside a mention's span breaks its
 * literal — `@**Sam**` can no longer resolve — and such a mention is dropped
 * rather than re-anchored, so a broken span cannot steal a same-text
 * duplicate's identity at send time.
 */
export function reconcileWrap(
  mentions: Mention[],
  start: number,
  end: number,
  mark: string,
): Mention[] {
  return mentions
    .filter((m) => {
      const at = m.offset;
      const to = at + m.text.length;
      // An insertion point strictly inside the span breaks the literal. An
      // insertion at a boundary (`start` at `at`, `end` at `to`) only shifts it.
      return !(start > at && start < to) && !(end > at && end < to);
    })
    .map((m) => {
      // Insertions at or before the mention's start shift it; an insertion at
      // its end (or later) does not.
      const shift =
        (start <= m.offset ? mark.length : 0) + (end <= m.offset ? mark.length : 0);
      return shift === 0 ? m : { ...m, offset: m.offset + shift };
    });
}

/**
 * The length of `s` in UTF-8 bytes, not UTF-16 units.
 *
 * The host defines a mention's `offset` as a byte position into the message
 * body, and `revalidate` checks it with `text.get(offset..)`. JavaScript string
 * indices are UTF-16, so the prefix up to a non-ASCII char is shorter in bytes —
 * `👍 ask @engineer` puts the mention at UTF-16 index 8 but byte index 12.
 * Sending the UTF-16 index would make the server look at the wrong characters
 * and demote the mention. The composer tracks UTF-16 indices internally (they
 * drive textarea and reconcile operations); this converts at the wire only.
 */
export function utf8ByteLength(s: string): number {
  return new TextEncoder().encode(s).length;
}

/** Whether the character at `idx` cleanly ends a mention — mirrors `closes_mention`. */
function closesMention(text: string, idx: number): boolean {
  const ch = text[idx];
  if (ch === undefined) return true;
  // Mirrors `closes_mention` in `src/runtime/mentions.rs`: only ASCII whitespace
  // or ASCII punctuation closes. JavaScript `\s` is wider and would truncate an
  // alias at a pasted non-breaking space the host keeps inside the mention.
  return /[ \t\n\r,;.!?:)\]}'"]/.test(ch);
}

/**
 * Every `@name` in `text` the directory can resolve to exactly one target.
 *
 * This is the composer side of the host's `extract_with_known`: longest alias
 * first, a name shared by two targets resolves to nobody, and code regions are
 * masked so an `@` inside backticks never names anyone. The composer sends the
 * picker's picks *and* what this resolves, because the host uses a non-empty
 * supplied list exclusively — a mention completed by hand (`@ceo ` — the query
 * closed on the finished name) never enters the picker's `mentions`, and a
 * partial supplied list would silently drop it.
 */
export function resolvableMentions(
  text: string,
  mentionables: readonly Mentionable[],
): Mention[] {
  const masked = stripCodeRegions(text);
  const out: Mention[] = [];
  let i = 0;
  while (i < masked.length) {
    if (masked[i] !== "@" || !opensMention(masked, i)) {
      i += 1;
      continue;
    }
    // `@#desk` is the desk-only spelling the host's fallback extraction also
    // accepts: the `#` is consumed and the match is narrowed to desk targets,
    // so no alias has to carry a `#` twin. Without the consume here, a desk
    // address typed by hand never enters the supplied list — and a loaded
    // directory sends that list explicitly, suppressing the fallback that
    // would have resolved it.
    const deskSpelling = masked[i + 1] === "#";
    const after = i + 1 + (deskSpelling ? 1 : 0);
    let best: { end: number; target: MentionTarget } | undefined;
    let ambiguous = false;
    for (const entry of mentionables) {
      if (deskSpelling && entry.target.kind !== "desk") continue;
      // The label is a spelling too — it is what the picker inserts, so a
      // hand-typed `@Jane Doe` must resolve even when no alias repeats it
      // (`aliasSet` already counts the label for the query-close rule).
      for (const alias of [entry.label.toLowerCase(), ...entry.aliases]) {
        const end = after + alias.length;
        if (end > masked.length) continue;
        if (masked.slice(after, end).toLowerCase() !== alias) continue;
        if (!closesMention(masked, end)) continue;
        if (!best || end > best.end) {
          best = { end, target: entry.target };
          // A strictly longer alias supersedes any ambiguity among shorter
          // candidates; only equal-length candidates can remain ambiguous.
          ambiguous = false;
        } else if (end === best.end && !sameTarget(best.target, entry.target)) {
          // Two targets claiming the same span: nobody gets pinged. A shorter
          // alias never overrides a longer one, so only equal lengths collide.
          ambiguous = true;
        }
      }
    }
    if (best && !ambiguous) {
      out.push({ target: best.target, text: masked.slice(i, best.end), offset: i });
      i = best.end;
      continue;
    }
    // Unresolved or ambiguous: leave it as text, and skip past this `@` so a
    // longer alias starting mid-word cannot re-match inside it.
    i = after;
  }
  return out;
}

/**
 * Whether two mention targets name the same thing.
 *
 * Both operands are narrowed explicitly. Checking `a.kind === "everyone"` alone
 * tells the compiler nothing about `b`, even after `a.kind !== b.kind` has been
 * ruled out — so the `id` read below needs both sides eliminated, not one.
 */
export function sameTarget(a: MentionTarget, b: MentionTarget): boolean {
  if (a.kind !== b.kind) return false;
  if (a.kind === "everyone" || b.kind === "everyone") return true;
  return a.id === b.id;
}

/**
 * A pattern matching exactly the spans in `mentions`, and nothing else.
 *
 * Returns {@link NEVER_MATCH} for an empty list, which is the point: an
 * `@word` is highlighted **only** when it corresponds to a mention the host
 * actually delivered. A chip is a claim that somebody was notified, so drawing
 * one over unresolved text would be a lie the reader cannot check.
 */
export function mentionRegex(mentions: Array<{ text: string }>): RegExp {
  if (mentions.length === 0) return NEVER_MATCH;
  const escaped = [...new Set(mentions.map((m) => m.text))]
    // Longest first, so `@Ann` cannot match inside `@Ann Lee`.
    .sort((a, b) => b.length - a.length)
    .map((t) => t.replace(/[.*+?^${}()|[\]\\]/g, "\\$&"));
  return new RegExp(`(${escaped.join("|")})`, "g");
}

/**
 * Turns the host's directory into picker rows for one channel.
 *
 * `inChannel` is what puts the people you are actually talking to at the top of
 * the list, and it is derived from the channel's own membership — which the
 * host only knows for teammates. Every signed-in person can see every desk, so
 * people are never "outside" a channel and are not marked either way.
 *
 * The `@everyone` row carries the host's spellings rather than a hard-coded
 * set, so the picker cannot offer a token the host would fail to resolve.
 */
export function mentionablesFor(
  directory: {
    agents: Array<{ id: string; name: string; role: string }>;
    people: Array<{ id: string; label: string; slug: string; avatar?: string }>;
    desks: Array<{ id: string; name: string; memberIds: string[] }>;
    everyone: { label: string; aliases: string[] };
  },
  channelMemberIds?: string[],
  selfId?: string,
): Mentionable[] {
  const inChannel = new Set(channelMemberIds ?? []);
  const agents: Mentionable[] = directory.agents
    .filter((a) => a.id !== selfId)
    .map((a) => ({
      target: { kind: "agent", id: a.id },
      // The display name is what an operator who has never read the manifest
      // will type; the host resolves agents by name and by id alike. Company
      // agents carry a `name`; the global defaults do not, so fall back to the id.
      label: a.name ?? a.id,
      aliases: [...new Set([a.id.toLowerCase(), a.name.toLowerCase()])],
      hint: a.role,
      inChannel: inChannel.has(a.id),
    }));
  // Two people can share a display name ("Sam"); the host mints each a distinct
  // slug precisely so one can be told from the other. Rows that would otherwise
  // be indistinguishable show the slug as their hint — a user picking "Sam" has
  // to be able to tell which Sam the row will ping.
  const labelCounts = new Map<string, number>();
  for (const p of directory.people) {
    labelCounts.set(p.label, (labelCounts.get(p.label) ?? 0) + 1);
  }
  const people: Mentionable[] = directory.people
    .filter((p) => p.id !== selfId)
    .map((p) => ({
      target: { kind: "user", id: p.id },
      label: p.label,
      avatar: p.avatar,
      aliases: [...new Set([p.label.toLowerCase(), p.slug])],
      hint:
        (labelCounts.get(p.label) ?? 0) > 1
          ? `Person — @${p.slug}`
          : "Person",
    }));
  const desks: Mentionable[] = directory.desks.map((d) => ({
    target: { kind: "desk", id: d.id },
    label: d.id,
    aliases: [...new Set([d.id.toLowerCase(), d.name.toLowerCase()])],
    hint:
      d.memberIds.length === 1
        ? `${d.name} — 1 teammate`
        : `${d.name} — ${d.memberIds.length} teammates`,
    memberIds: d.memberIds,
  }));
  const everyone: Mentionable = {
    target: { kind: "everyone" },
    label: directory.everyone.label,
    aliases: directory.everyone.aliases.map((a) => a.toLowerCase()),
    hint: "Notify everyone here",
  };
  return [...agents, ...people, ...desks, everyone];
}

/**
 * The teammates a draft would address who are **not** on this channel.
 *
 * Mentioning somebody who cannot see the channel is a real mistake and a silent
 * one: the message sends, the chip renders, and the person never appears. The
 * composer warns before the send rather than after.
 *
 * People are never returned. Every signed-in person can see every desk, so
 * "outside this channel" does not apply to them — only to teammates, whose desk
 * membership is real.
 *
 * A desk mention is judged by its blast radius: each of the desk's members is
 * compared against the channel, so picking a desk whose members are all outside
 * warns exactly as naming each of them directly would. `mentionables` supplies
 * that membership (the picker rows carry `memberIds` on desk rows); without it
 * a desk mention is left alone rather than guessed about.
 */
export function mentionsOutsideChannel(
  mentions: Mention[],
  channelMemberIds: string[] | undefined,
  mentionables?: readonly Mentionable[],
): string[] {
  // Unknown membership is not empty membership: a DM and a fallback desk both
  // report none, and warning about every mention there would be noise.
  if (!channelMemberIds) return [];
  const members = new Set(channelMemberIds);
  const deskMembers = new Map<string, string[]>();
  for (const entry of mentionables ?? []) {
    if (entry.target.kind === "desk") {
      deskMembers.set(entry.target.id, entry.memberIds ?? []);
    }
  }
  return mentions
    .flatMap((m) => {
      if (m.target.kind === "agent") {
        return members.has(m.target.id) ? [] : [m.target.id];
      }
      if (m.target.kind === "desk") {
        return (deskMembers.get(m.target.id) ?? []).filter(
          (id) => !members.has(id),
        );
      }
      return [];
    })
    .filter((id, i, all) => id && all.indexOf(id) === i);
}
