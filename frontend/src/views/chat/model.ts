// The chat workspace's data model: channels, direct messages, and the grouping
// rules the timeline reads. Everything here is pure — the view owns the state.

import type { ApprovalSummary, DeskDto, OperatorChannelDto, Verdict } from "@/api/types";
import { isAnyBudgetPauseNotice, parseBudgetPauseAgent } from "@/hooks/use-events";
import {
  clearTaskCard,
  generalAwareChannel,
  MAIN_THREAD_ID,
  type ChatMessage,
  type Reaction,
} from "@/lib/chat";
import {
  deskClaimsGeneralChannel,
  GENERAL_CHANNEL,
  isGeneralChannel,
  type Desk,
} from "@/lib/desks";
import { initials as nameInitials, type TeamMember } from "@/lib/team";

/**
 * A host desk (`GET .../desks`), shaped into the console's `Desk`. The host
 * has no separate channel-slug or blurb field, so the slug is derived from
 * the desk's name and the blurb falls back to its description — the id is
 * the one field that must survive untouched, since it doubles as the chat
 * thread id `send` addresses.
 *
 * `members` / `overlayMembers` come through as the host sent them, order
 * included — `members[0]` is the desk's lead, and the rest is the hierarchy the
 * company declared. Dropping them here is what made every channel show the
 * whole company (issue #369).
 */
export function deskFromDto(d: DeskDto): Desk {
  const slug = d.name.trim().toLowerCase().replace(/[^a-z0-9]+/g, "-").replace(/^-|-$/g, "");
  return {
    id: d.id,
    channel: slug || d.id,
    name: d.name,
    blurb: d.description ?? "",
    members: d.members,
    overlayMembers: d.overlayMembers,
    overlayCreated: d.overlayCreated,
    responder: d.responder,
  };
}

/**
 * Every channel's transcript, keyed by channel id. Owned by `AppShell`, not
 * `ChatView`, so a transcript survives `ChatView` unmounting when the operator
 * steps into Tasks, Settings, or any other view and comes back.
 */
export type Transcripts = Record<string, ChatMessage[]>;

/**
 * The message id of the MOST RECENT budget-pause notice per agent,
 * COMPANY-WIDE — not scoped to one channel (issue #1846 review, Codex
 * #3865395879).
 *
 * The backend keeps at most one parked marker per agent, full stop: a pause
 * in channel A followed by a pause for the SAME agent in channel B overwrites
 * A's marker with B's, regardless of which channel either happened in. This
 * used to be computed from a single channel's `items` inside `MessageTimeline`
 * — correct for the channel that is actually open, but wrong for any OTHER
 * channel holding an older notice for an agent who has since paused again
 * elsewhere: reopening that channel, its own last notice still reads as the
 * newest ONE IT has seen, so the "Add credits & resend" button stayed
 * enabled — and clicking it silently redeemed the newer, different-channel
 * marker and resent that unrelated message under the stale card.
 *
 * Folding over every channel in `transcripts` (which `AppShell` keeps live
 * for the whole company over SSE, not merely the currently open one) is what
 * actually matches the backend's one-marker-per-agent truth. Sorted by each
 * message's own `at` timestamp rather than by scan order, because iterating
 * one channel's array to completion before moving to the next would NOT
 * yield cross-channel chronological order on its own.
 *
 * Scanned with `isAnyBudgetPauseNotice`, NOT `isBudgetPauseNotice` (issue
 * #1906). The narrow check is the render-time one — "does this notice get an
 * Add-credits button" — and filtering the supersession map through it made
 * every NO-RESEND notice invisible here, while the marker it parked still
 * overwrote the previous one on the host. maya pauses on an interactive turn
 * (redeemable notice N1, marker M1); an approval for maya is then approved,
 * its continuation runs through `run_steered_background`, pauses, and parks M2
 * (`background: true`) over M1 with a no-resend notice. N1 was still this
 * map's answer for maya, so its CTA stayed enabled and clicking it sent
 * `?id=M1.id` — a marker that no longer exists — for a `RedeemMatch::Stale`
 * 409 that refreshing could never clear. A pause is a pause for supersession
 * purposes whether or not the operator gets a button for it.
 */
export function latestBudgetPauseMessageIdByAgent(transcripts: Transcripts): Map<string, string> {
  const latest = new Map<string, { messageId: string; at: number }>();
  for (const messages of Object.values(transcripts)) {
    for (const message of messages) {
      if (!isAnyBudgetPauseNotice(message.text)) continue;
      const agentId = parseBudgetPauseAgent(message.text);
      if (agentId == null) continue;
      const seen = latest.get(agentId);
      if (seen == null || message.at >= seen.at) {
        latest.set(agentId, { messageId: message.id, at: message.at });
      }
    }
  }
  const out = new Map<string, string>();
  for (const [agentId, { messageId }] of latest) out.set(agentId, messageId);
  return out;
}

/**
 * Folds one `GET …/budget-pause` read into `ChatView`'s
 * `budgetPauseMarkerByNotice` cache (issue #1846 review, Codex #3868962374)
 * — the notice-render-time read that replaces redeeming off a live re-read
 * at click time. Pure so it can be pinned directly; there is no
 * component-test harness in this project to mount the effect it backs (see
 * `budget-pause-notice.test.ts`'s header doc for the same constraint).
 *
 * Never overwrites an id already cached for `messageId` — the FIRST
 * successful read for a given notice is the one that landed closest to when
 * the card actually appeared, which is exactly the property this cache
 * exists to buy: a later read racing a background re-park would otherwise
 * silently replace the correct id with a newer, unrelated one.
 */
export function mergeBudgetPauseMarkerRead(
  prev: Map<string, string>,
  messageId: string,
  markerId: string,
): Map<string, string> {
  if (prev.has(messageId)) return prev;
  const next = new Map(prev);
  next.set(messageId, markerId);
  return next;
}

/**
 * The marker id `redeemBudgetPause` should send for a click on `noticeMessageId`
 * (issue #1846 review, Codex #3868962374): the notice-render-time cached read
 * when one landed in time, else `liveFallback` — a live read performed AT
 * click time, for the narrow case the cache has nothing yet (a click landing
 * faster than the render-time `GET` resolved). Falling back rather than
 * refusing the click keeps this no worse than the pre-fix behaviour, which
 * always read live at click time.
 */
export function budgetPauseRedeemId(
  noticeMessageId: string,
  markerByNotice: Map<string, string>,
  liveFallback: string | undefined,
): string | undefined {
  return markerByNotice.get(noticeMessageId) ?? liveFallback;
}

/**
 * How far a channel's persisted history has got, per channel.
 *
 * {@link Transcripts} cannot answer this on its own: an absent key and a key
 * holding `[]` both read as "no messages", and the timeline coerces the two
 * together the moment it does `transcripts[id] ?? []`. So "nobody has asked the
 * host yet" is indistinguishable from "the host says this channel is empty",
 * and the timeline printed the second while the first was true — the reload
 * flash issue #934 describes.
 */
export type HistoryStatus = "loading" | "ready";

export interface HistoryHydration {
  /**
   * Whether the desks/roster pass has finished marking every channel it is
   * going to hydrate.
   *
   * Needed because `ChatView` resolves its own desk list independently of the
   * shell's, and can therefore render a channel before the shell's pass has
   * reached it. Without this, that window has no entry in `byChannel` and looks
   * exactly like a channel nothing will ever hydrate.
   */
  discovered: boolean;
  /** Channel id → whether its `chat/history` request has settled. */
  byChannel: Record<string, HistoryStatus>;
}

/** Before a company's rehydration pass has begun: everything is still pending. */
export const HISTORY_UNSTARTED: HistoryHydration = { discovered: false, byChannel: {} };

/**
 * No rehydration is happening or ever will — for a `ChatView` mounted without a
 * shell behind it. The distinction from {@link HISTORY_UNSTARTED} is the whole
 * point: this one resolves every channel to "ready", so a caller that does not
 * track hydration renders exactly as it did before, rather than spinning on a
 * pass that is never coming.
 */
export const HISTORY_UNTRACKED: HistoryHydration = { discovered: true, byChannel: {} };

/**
 * Whether we know enough about `channelId` to state that it is empty.
 *
 * The three cases, and why the last one is `discovered` rather than `false`: a
 * channel with a status answers for itself; a channel with none *after* the
 * pass has run is one nothing will hydrate (a console-only teammate, a host
 * with no `chat/history`), and holding a spinner on it forever is worse than
 * the wrong claim this exists to prevent.
 */
export function historyReady(hydration: HistoryHydration, channelId: string): boolean {
  const status = hydration.byChannel[channelId];
  if (status) return status === "ready";
  return hydration.discovered;
}

export type ChannelKind = "channel" | "dm";

export interface Channel {
  id: string;
  /** The bare name — rendered after a `#` for channels, plain for DMs. */
  name: string;
  /**
   * How the company side signs its messages here. A channel's name is a slug
   * (`front-desk`); its voice is who is speaking (`Front desk`).
   */
  voice?: string;
  kind: ChannelKind;
  /** One line under the title, and the tooltip on it. */
  purpose: string;
  /** Private channels wear a lock instead of a hash. */
  private?: boolean;
  /** Avatar tone key; DMs and desk channels both carry one. */
  tone?: string;
  /** The roster entry behind a DM, when there is one. */
  member?: TeamMember;
  /**
   * Who is in this channel, as roster teammate **ids** in the desk's own order
   * (lead first). Ids rather than resolved `TeamMember`s so this model stays
   * pure and a channel never goes stale when the roster reloads — resolve with
   * {@link channelMembers}.
   *
   * Absent for DMs (a two-person line needs no list) and for the static
   * fallback desks, which have no membership concept; a consumer that finds it
   * absent falls back to the whole roster (issue #369).
   */
  memberIds?: string[];
  /**
   * Whether this is the built-in **Operator** system channel (issue #1757) — a
   * read-only aggregation feed of workflow reports. The composer is disabled for
   * it and it offers no membership editing.
   */
  system?: boolean;
  /**
   * Whether this channel has **no lead** (issue #1835): an `auto` desk, whose
   * answerer is picked per message. `memberIds[0]` carries no rank here, so a
   * consumer must not badge it — the host's own `desk_lead` is `None` for such
   * a channel by definition. Absent/false for every lead desk and DM.
   */
  leadless?: boolean;
}

export interface ChannelSection {
  id: string;
  label: string;
  channels: Channel[];
}

/**
 * The channel list.
 *
 * `desks` become the `#channels` — they are the standing lines you can
 * address, and each already carries a name, a blurb, and a tone. Defaults to
 * `lib/desks.ts`'s static set for a host that doesn't expose `.../desks` yet
 * (issue #53); the caller fetches the real ones and passes them in once they
 * land, so a company's own desks show up instead of the generic
 * strategy/creative/front-desk trio. A DM appears only after it has a
 * transcript, newest conversation first; the compose picker still exposes the
 * complete roster for starting one.
 *
 * Both kinds post to the same company endpoint. A channel scopes a transcript
 * and gives the company side a stable identity; it is not a separate backend.
 */
/**
 * The built-in `#general` channel: the company-wide line, in every company,
 * from first boot (issue #1743).
 *
 * # It is not a desk, and that is the point
 *
 * Every other channel here is a desk, and a desk has a lead and a hierarchy.
 * "Everyone" has neither, so `#general` is deliberately absent from
 * `GET .../desks` — which is what keeps every desk-shaped surface honest for
 * free: the org chart, the assignee picker and the desk counts all read that
 * route, so none of them can offer this channel a lead, a seat, a rename or a
 * delete. The console renders no edit or delete affordance on it because there
 * is nothing to render one from, not because a button is hidden. The host
 * refuses those writes anyway, with a reason (`GENERAL_CHANNEL_IMMUTABLE`).
 *
 * # Membership is derived, never stored
 *
 * `memberIds` is the roster this render was handed, in roster order. Nothing
 * anywhere records who is in `#general`, so a teammate added a minute ago is a
 * member with no write and the two cannot drift. The host derives the same set
 * the same way when it expands `@everyone` here.
 *
 * # Who answers a message that mentions nobody
 *
 * The orchestrator — the same teammate that has always answered the company's
 * main line, one turn per message. `isOrchestrator` is the host's own roster
 * rule read off `GET .../team`, never re-derived from `tier`; a host that does
 * not answer it leaves every row `undefined`, and the purpose line then simply
 * does not make the claim.
 */
function generalChannel(members: TeamMember[]): Channel {
  const orchestrator = members.find((m) => m.isOrchestrator);
  return {
    id: MAIN_THREAD_ID,
    name: GENERAL_CHANNEL,
    voice: orchestrator?.name ?? "Your company",
    kind: "channel",
    purpose: orchestrator
      ? `Everyone's here. ${orchestrator.name} picks up anything you don't @-mention.`
      : "Everyone's here — the whole company on one line",
    memberIds: members.map((m) => m.id),
  };
}

export function buildChannels(
  members: TeamMember[],
  // Defaults to no desks, not to the fabricated trio. The parameter exists so
  // a caller that has not read `/desks` yet can still build the rail's roster
  // half; standing in three desks the company never declared is the bug this
  // default used to carry into every such caller.
  desks: Desk[] = [],
  transcripts: Transcripts = {},
): ChannelSection[] {
  // A desk that answers to a General spelling owns the company-wide line, and
  // the built-in channel steps aside for it rather than doubling it.
  //
  // This is the host's own rule, not a console one: `is_general_channel`
  // (`src/server/operator.rs`) is guarded on `!record.desk_exists(desk_id)`, so
  // a blueprint that declares `[[group_chat]] id = "general"` — or `"main"` —
  // keeps its desk, its lead, its writes, and `responder_for` routes messages
  // addressed there to that lead. A rail that showed a second, lead-less
  // `#general` beside it, or hid the desk and named the orchestrator as who
  // answers, would state something the host does not do.
  //
  // Desk *creation* refuses every General spelling, so such a desk can only
  // come from a blueprint — and `defaultDesks()` no longer fabricates one, so
  // "a desk claims it" is now a fact about the company rather than about which
  // fallback set the console happened to be holding.
  const claimed = desks.some(deskClaimsGeneralChannel);
  const channels: Channel[] = [
    ...(claimed ? [] : [generalChannel(members)]),
    ...desks.map((d) => ({
      id: d.id,
      name: d.channel,
      voice: d.name,
      kind: "channel" as const,
      // An `auto` channel with no blurb of its own states its routing rule —
      // the honest line about who answers, in place of a rank nothing confers
      // (issue #1835). An operator-written blurb still wins.
      purpose:
        d.blurb ||
        (d.responder === "auto" ? "Best fit picks up anything you don't @-mention" : d.blurb),
      tone: d.tone,
      memberIds: d.members,
      leadless: d.responder === "auto" || undefined,
    })),
  ];

  const dms = directMessageChannels(members)
    .filter((dm) => (transcripts[dm.id]?.length ?? 0) > 0)
    .sort((a, b) => latestMessageAt(transcripts[b.id]) - latestMessageAt(transcripts[a.id]));

  return [
    { id: "channels", label: "Channels", channels },
    { id: "dms", label: "Direct messages", channels: dms },
  ];
}

/**
 * Shape `GET {scope}/operator-channel`'s response into the console's
 * `Channel` (issue #1757 rework). A read-only system channel — the composer
 * is disabled for it and it offers no membership editing — distinct from
 * every desk-backed channel `buildChannels` produces.
 */
export function operatorChannelFrom(dto: OperatorChannelDto): Channel {
  return {
    id: dto.id,
    name: dto.name,
    kind: "channel",
    purpose: dto.description,
    system: true,
  };
}

/**
 * Whether `value` actually has the `OperatorChannelDto` shape — a runtime
 * check, not just a type assertion. Callers hold this at the network
 * boundary: a client stub/proxy that resolves every unlisted method to `[]`
 * (a common test fixture pattern in this codebase) would otherwise satisfy
 * TypeScript at the call site and only fail once `operatorChannelFrom` reads
 * `dto.description` off an array and hands `channelSubtitle` an `undefined`
 * `purpose` to `.trim()`. Treated the same as a fetch failure by callers:
 * degrade to no pinned row rather than crash the view.
 */
export function isOperatorChannelDto(value: unknown): value is OperatorChannelDto {
  if (typeof value !== "object" || value === null || Array.isArray(value)) return false;
  const dto = value as Partial<OperatorChannelDto>;
  return (
    typeof dto.id === "string" && typeof dto.name === "string" && typeof dto.description === "string"
  );
}

/**
 * The pinned Operator row as its own {@link ChannelSection}, meant to be
 * appended *after* every other section (issue #1757 rework) so the first
 * writable desk still wins the "open by default" pick — see `buildChannels`'s
 * `channels` section, which a caller composes ahead of this one.
 *
 * Callers at the network boundary MUST validate with {@link isOperatorChannelDto}
 * before reaching here — this function does not re-check, and `operatorChannelFrom`
 * reading `dto.description` off a shape that only satisfied the type assertion
 * (never the runtime one) is exactly the crash `isOperatorChannelDto`'s own doc
 * warns about. `ChatView`'s only production call site holds this invariant by
 * construction: its `operator` state is set from `isOperatorChannelDto(dto) ?
 * dto : null` and this function is only ever called on the non-null branch.
 */
export function operatorSection(dto: OperatorChannelDto): ChannelSection {
  return { id: "operator", label: "Operator", channels: [operatorChannelFrom(dto)] };
}

/** Every roster teammate as a DM target, including conversations not yet started. */
export function directMessageChannels(members: TeamMember[]): Channel[] {
  return members.map((m) => ({
    id: dmChannelId(m),
    name: m.name,
    kind: "dm" as const,
    // The teammate's **description**, which is the field parallel to a desk's
    // `blurb` above — both answer "what is this line for", and neither repeats
    // what the title already said. This used to read `m.role`, an identity
    // field in a description slot, and that is precisely what made the header
    // say the same words twice (issue #1180): `fromDto` falls back
    // `dto.name?.trim() || dto.role`, so a company that names roles rather than
    // people has name === role, and the title and the slot after the divider
    // resolved to one string. The role is still the fallback — for a teammate
    // the host *did* name it is a real second fact — and {@link channelSubtitle}
    // is what declines to render even that when it just echoes the title.
    purpose: m.description.trim() || m.role,
    tone: m.tone,
    member: m,
  }));
}

/** A DM target addressed by `id`, whether or not it is in the rail yet. */
export function directMessageForId(members: TeamMember[], id: string | null): Channel | null {
  if (!id) return null;
  return directMessageChannels(members).find((channel) => channel.id === id) ?? null;
}

function latestMessageAt(messages: ChatMessage[] | undefined): number {
  return messages?.reduce((latest, message) => Math.max(latest, message.at), 0) ?? 0;
}

/**
 * A DM's channel id, and so its URL.
 *
 * Keyed on the teammate's **id**, which is now the one stable thing about them
 * (issue #364). A host roster entry has always had a stable agent id; what was
 * missing was a stable id for a console-invented teammate, and `lib/team.ts`'s
 * counter was why this used to key on the name instead.
 *
 * Keying on the name had a cost that only shows up later: renaming a teammate
 * silently moved their DM to a new URL and orphaned every message already
 * journaled under the old one. An id does not change when a person's name does,
 * which is the entire reason to prefer it.
 */
export function dmChannelId(member: TeamMember): string {
  return `dm:${member.id}`;
}

/**
 * The **host thread** a teammate's DM is addressed on — not always the same
 * string as {@link dmChannelId}, which is its console-local channel id.
 *
 * Issue #364 re-keyed DMs onto the bare teammate id, and for every ordinary
 * teammate that is still what this answers. The exception is a teammate whose
 * id is itself a General spelling: the host folds that bare key to the
 * company-wide line before it ever reaches the roster — deliberately, so `main`
 * cannot be captured by a teammate called `main` — and answers it as the
 * orchestrator. Addressed bare, that DM was writable and unreadable at once.
 *
 * So exactly that one teammate is addressed prefixed, which `chat_responder`
 * unwraps and resolves (`chat_responder("dm:main") == Some("main")`).
 *
 * **Every seam that turns a roster member into a thread id has to ask this**,
 * not only the sender: the live thread → channel map, the rehydration targets
 * that recover history after a reload, and the Approvals page resolving an
 * origin back to its conversation. Any one of them left on the bare id puts
 * that DM's replies, its recovered history, or its approval link back on the
 * company's line (issue #1743).
 */
export function dmThreadId(member: TeamMember): string {
  return isGeneralChannel(member.id) ? dmChannelId(member) : member.id;
}

/** The teammate a thread addresses, whether it is bare or `dm:`-prefixed. */
export function memberForThread(members: TeamMember[], threadId: string): TeamMember | null {
  return (
    members.find((m) => dmThreadId(m) === threadId) ??
    members.find((m) => dmChannelId(m) === threadId) ??
    null
  );
}

/**
 * The name-derived DM id this console minted before issue #364.
 *
 * Kept for one release, and for one purpose: a `#/chat/dm:ada-1f3k` link that
 * somebody bookmarked or pasted into a ticket still has to land on Ada's DM.
 * Only {@link resolveDmChannelId} calls it — nothing addresses a channel by it,
 * so no new state is ever written under a legacy id.
 */
export function legacyDmChannelId(member: TeamMember): string {
  const name = member.name.trim();
  const slug = name.toLowerCase().replace(/[^a-z0-9]+/g, "-").replace(/^-|-$/g, "");
  return `dm:${slug ? `${slug}-` : ""}${nameHash(name)}`;
}

/**
 * The current DM channel id a URL segment names, or `null` when it names no
 * DM this company has.
 *
 * Resolves the current id first, then the pre-#364 name-derived form, so an old
 * link keeps working without the old id ever becoming addressable again.
 */
export function resolveDmChannelId(id: string, members: TeamMember[]): string | null {
  if (!id.startsWith("dm:")) return null;
  const match = members.find(
    (m) => dmChannelId(m) === id || legacyDmChannelId(m) === id,
  );
  return match ? dmChannelId(match) : null;
}

/**
 * The channel id a `#/chat/<id>` hash segment names, with the URL escaping
 * undone.
 *
 * The hash router hands views its raw segments without decoding, while hrefs
 * that mint channel links — an approval card's "Open the conversation" pill —
 * write them with `encodeURIComponent`, so a DM id arrives as `dm%3A<agent-id>`
 * rather than `dm:<agent-id>`. Decode here, the same boundary
 * `taskIdFromSegment` keeps for `#/tasks/<id>`.
 *
 * On a malformed escape the raw segment comes back rather than `null`: a
 * typo'd address should still surface as an unknown channel (issue #370's
 * notice), not silently collapse onto the fallback.
 */
export function channelIdFromSegment(segment: string | null): string | null {
  if (!segment) return null;
  try {
    return decodeURIComponent(segment);
  } catch {
    return segment;
  }
}

function nameHash(name: string): string {
  let hash = 0;
  for (let i = 0; i < name.length; i++) hash = (hash * 31 + name.charCodeAt(i)) | 0;
  return (hash >>> 0).toString(36);
}

/**
 * The chat channel a **host thread id** belongs to, or `null` when this
 * company has no channel that owns it.
 *
 * This is the one subtle rule in the chat's addressing, kept in one place so a
 * caller cannot get it wrong twice. A desk's channel id *is* its thread id —
 * {@link deskFromDto} leaves `DeskDto.id` untouched precisely so addressing the
 * channel routes to that desk. A DM's is not: its channel id is the
 * console-local {@link dmChannelId}, while the thread id the host journals
 * under (and `chat` / `chat/history` take) is the roster teammate's agent id.
 *
 * Anything routing a host-side event — which always names a *thread* — into
 * {@link Transcripts} has to make that distinction. Issue #367 is what the
 * console looks like when it doesn't.
 */
export function channelIdForThread(
  threadId: string,
  desks: Desk[],
  members: TeamMember[],
): string | null {
  if (desks.some((d) => d.id === threadId)) return threadId;
  // The built-in `#general` channel is in no desk list, so it has to be
  // resolved by name (issue #1743). The host journals this one conversation
  // under four ids — `""`, `main`, `General`, `general` — and folds them on
  // read; a thread carrying any of them belongs to the one channel that
  // renders it. Without this, an approval raised on the company's main line
  // matched no channel and stayed stranded on the Approvals page, and an
  // unaddressed live message had nowhere in `Transcripts` to land.
  //
  // Checked before the roster, same order the host resolves in
  // (`responder_for`: desk, then the General fold, then the roster) — so the
  // console never claims a thread belongs somewhere the host would answer
  // from somewhere else.
  if (isGeneralChannel(threadId)) {
    return generalChannelId(desks);
  }
  const member = members.find((m) => m.id === threadId);
  if (member) return dmChannelId(member);
  // The prefixed form, last, exactly as `chat_responder` unwraps it.
  //
  // A teammate whose id *is* a General spelling cannot be addressed bare — the
  // fold above answers first and the company's line wins, which is deliberate
  // (`chat_responder("main") == None`, "a teammate called `main` does not
  // inherit the company's line"). `ChatView` therefore addresses that one DM as
  // `dm:<id>`, which the host does answer. The frames it emits carry that
  // prefixed key, so without this arm they resolved to no channel at all and
  // the reply never appeared — the DM was writable and unreadable at once.
  const prefixed = threadId.startsWith("dm:") ? threadId.slice("dm:".length) : null;
  const dmMember = prefixed ? members.find((m) => m.id === prefixed) : null;
  return dmMember ? dmChannelId(dmMember) : null;
}

/**
 * The channel the company-wide line actually renders in.
 *
 * `main` — the built-in channel — in every ordinary company. A blueprint that
 * declares a `[[group_chat]]` under a General id is grandfathered by the host
 * (`is_general_channel` is guarded on `!record.desk_exists`), and
 * {@link buildChannels} then lets that desk own the line and adds no built-in
 * channel beside it; here that desk's own id is the answer.
 *
 * One place, because two answers to "where does the main line render" is
 * precisely how a message ends up somewhere nothing is listening.
 */
function generalChannelId(desks: Desk[]): string {
  return desks.find(deskClaimsGeneralChannel)?.id ?? MAIN_THREAD_ID;
}

/**
 * The channel that renders `threadId`, given the shell's thread → channel map.
 *
 * The map holds one entry per id the company can be addressed on, which cannot
 * cover the General spellings exhaustively: the host compares them
 * case-insensitively (`is_general_chat`) and then emits on live events the raw
 * id it was addressed with, so an API client posting to `MAIN` produces frames
 * keyed `MAIN` and a four-literal map misses them. A missed frame is not a
 * cosmetic loss — the reply and the working indicator simply do not appear
 * until polling happens to recover the durable history.
 *
 * So: exact match first, then the same question the host asks.
 */
export function channelForThread(
  map: Readonly<Record<string, string>>,
  threadId: string,
): string | null {
  // One implementation, in `lib/chat.ts`, because `dispatchMarkerPlacement`
  // lives there and has to resolve a thread the same way (issue #1743). A
  // second copy here is how three of the four live-frame consumers ended up
  // indexing the map directly and missing every casing the host accepts.
  return generalAwareChannel(map, threadId);
}

export function findChannel(sections: ChannelSection[], id: string | null): Channel | null {
  if (!id) return null;
  for (const s of sections) {
    const hit = s.channels.find((c) => c.id === id);
    if (hit) return hit;
  }
  return null;
}

/**
 * The first channel across all sections, or `null` when there are none.
 *
 * The last-resort selection, so the chat never renders blank while any channel
 * exists (issue #366), and the address of last resort for a line with no
 * channel of its own (issue #368). Both used to reach for a literal `"main"`
 * instead — an id carried only by the first *fallback* desk, so it matched
 * nothing at all on a company with desks of its own.
 */
export function firstChannel(sections: ChannelSection[]): Channel | null {
  for (const s of sections) {
    if (s.channels.length > 0) return s.channels[0];
  }
  return null;
}

/**
 * A channel's own members, resolved against the roster — `null` when the
 * channel names no membership (a DM, or a fallback desk), which is the caller's
 * cue to fall back to the whole roster rather than draw an empty pane.
 *
 * Maps over the **ids**, not over the roster: the desk's order is meaningful —
 * `memberIds[0]` is the lead — and filtering the roster would silently reorder
 * everyone into roster order and lose that. An id with no roster row (a
 * teammate removed since the desks were fetched) drops out rather than
 * rendering a placeholder for somebody who isn't there.
 */
export function channelMembers(channel: Channel, roster: TeamMember[]): TeamMember[] | null {
  if (!channel.memberIds) return null;
  const byId = new Map(roster.map((m) => [m.id, m]));
  return channel.memberIds
    .map((id) => byId.get(id))
    .filter((m): m is TeamMember => m !== undefined);
}

/** How a channel is titled in the header and the rail. */
export function channelTitle(channel: Channel): string {
  return channel.kind === "dm" ? channel.name : `#${channel.name}`;
}

/**
 * The line that goes *beside* the title — the muted slot after the header's
 * divider, the rail row's tooltip, the conversation intro's clause — or `null`
 * when there is nothing to say that the title has not already said.
 *
 * `null` rather than the empty string, and a rule rather than a DM special
 * case. A subtitle exists to add a second fact; one that repeats the first is
 * not a hierarchy, it is the same word twice with two type styles, and the
 * honest render of "nothing more to say" is nothing. Issue #1180 is what that
 * looks like when it ships: every agent in a company that declares roles and no
 * names read `Backend Engineer │ Backend Engineer` across the top of its DM.
 *
 * The comparison is case- and whitespace-insensitive because the duplicate is a
 * duplicate to a reader either way — a manifest whose description restates the
 * role in sentence case is the same non-fact as one that restates it verbatim.
 *
 * Kind-agnostic on purpose. A DM is where this bites today, but a desk whose
 * blurb is just its own name is the identical duplicate under `#`, and a rule
 * that only fires for DMs would let that one through. Channels are otherwise
 * untouched: a blurb that says something the slug does not — which is every
 * desk that bothered to write one — comes back exactly as before.
 */
export function channelSubtitle(channel: Channel): string | null {
  const purpose = channel.purpose.trim();
  if (!purpose) return null;
  // Collapse runs of internal whitespace too, not just the outer trim — a
  // manifest description copy-pasted from the role with a doubled space or a
  // stray newline in the middle is still the same duplicate to a reader, and
  // the doc comment above promises "whitespace-insensitive" without
  // qualification.
  const normalize = (s: string) => s.trim().replace(/\s+/g, " ").toLowerCase();
  if (normalize(purpose) === normalize(channel.name)) return null;
  return purpose;
}

/**
 * The line under the identity block at the top of an empty transcript.
 *
 * Pure, exported and here rather than inline in `MessageTimeline` because it is
 * four branches of prose over two nullable inputs, and prose defects are
 * invisible to a type checker. The one this arrived with: the DM branch used to
 * append a full stop by hand, which was right while the subtitle was a role
 * (`Backend Engineer`) and produced "…and services.." the moment issue #1180
 * made it a description that brings its own punctuation. `sentence` is the rule
 * for that, so every branch goes through it.
 *
 * `loading` renders the subtitle alone: both finished sentences are positive
 * claims that the channel has no history, and neither may be made before the
 * host has answered (issue #934).
 */
export function channelIntroSentence(channel: Channel, loading: boolean): string {
  const subtitle = channelSubtitle(channel);
  if (loading) return subtitle ? sentence(subtitle) : "";
  if (channel.kind === "dm") {
    // No subtitle drops the clause, not the sentence: where you are is still
    // worth saying, the tautology after it is not.
    return subtitle
      ? `This is the start of your direct message with ${channel.name} — ${sentence(lower(subtitle))}`
      : `This is the start of your direct message with ${channel.name}.`;
  }
  return `This is the very beginning of ${channelTitle(channel)}.${subtitle ? ` ${sentence(subtitle)}` : ""}`;
}

/** Lowercases the first character only, for a clause continuing a sentence. */
function lower(s: string): string {
  return s.charAt(0).toLowerCase() + s.slice(1);
}

/** Terminates `s` with a full stop unless it already ends in punctuation. */
function sentence(s: string): string {
  const t = s.trim();
  return /[.!?]$/.test(t) ? t : `${t}.`;
}

/**
 * The face a DM wears — the `TeammateAvatar` seed for the teammate on the
 * other end — or `null` for anything that has no face: a channel, and a DM
 * with no roster entry behind it (both of those wear a glyph instead).
 *
 * One function rather than the same props written out at each call site,
 * because the rail row and the header sit on screen together, and a call site
 * that seeded its mascot differently from another would draw a *different*
 * face for the same person a few pixels away — worse than the generic glyph
 * the header used to show (issue #1170). Deriving both from here is what
 * makes that drift impossible rather than merely unlikely.
 *
 * `avatar` is `channel.member.avatar` — the id-seeded mascot key `fromDto`
 * already computed onto the roster entry (issue #1185) — rather than
 * `channel.name`: a teammate's face must survive a rename, and `TeamMember`
 * already carries the seed that does that. `tone` needs no such rerouting;
 * `buildChannels` already sets it from `member.tone`, which was id-seeded from
 * the start.
 */
export function dmFace(channel: Channel): { name: string; tone?: string; avatar?: string } | null {
  if (channel.kind !== "dm" || !channel.member) return null;
  return { name: channel.name, tone: channel.tone, avatar: channel.member.avatar };
}

/**
 * Whether this chat target's composer offers "Do it once" / "Build me the
 * workflow" (issues #580, #845).
 *
 * True for every real chat target — a channel line and a DM can each open a
 * board card, and the card is what routes a `workflow` request to the builder
 * pass. It is deliberately a total function over [`ChannelKind`] rather than an
 * inline `kind === "channel"`, so adding a kind is a decision someone makes here
 * instead of a control that silently fails to appear.
 *
 * Not every composer asks: the thread and copilot composers pass nothing and
 * keep their plain `(text)` `onSend`. A thread reply continues a message that
 * already made this choice, and a copilot line is about one graph rather than a
 * request to the company.
 *
 * #580 shipped the control on channels only. Nothing downstream was ever scoped
 * to channels — the chat route reads `deliverable` off the payload whatever
 * thread it came from — so the asymmetry lived entirely in the caller, and a DM
 * asking for a workflow had no way to say so. It went as a `once` card, was
 * dispatched to a desk agent holding no authoring tool, and came back a refusal.
 */
export function offersDeliverableChoice(kind: ChannelKind): boolean {
  switch (kind) {
    case "channel":
    case "dm":
      return true;
  }
}

/* ---- senders ---- */

export type SenderKind = "you" | "company" | "agent" | "system";

export interface Sender {
  /** Stable identity, so consecutive lines from one voice group together. */
  key: string;
  name: string;
  kind: SenderKind;
  tone?: string;
  /**
   * The avatar reference (`TeamMember.avatar`) when the sender resolves to a
   * roster teammate, and your own when the sender is you.
   *
   * Undefined for "system", and for an agent voice `senderOf` could not match
   * against the roster — `TeammateAvatar` falls back to seeding on `name` in
   * both of those cases, same as before issue #1185.
   */
  avatar?: string;
  /**
   * The roster agent id behind this voice, when there is one — what a click on
   * the face opens the profile panel on (issue #1653).
   *
   * Set only on a voice that actually **matched** the roster, never on the
   * channel slug that seeded the face. A desk-originated cross-post carries a
   * desk id in that slot (see `api/types.ts` on `thread`), and opening a
   * teammate profile on a desk id would ask the host for an agent that does not
   * exist.
   */
  agentId?: string;
}

/** Channel names the host uses for its own voice rather than a named agent. */
const COMPANY_VOICE = new Set(["operator", "console", "chat", "owner", ""]);

/**
 * Who said a line, within a channel.
 *
 * The company side wears the channel's identity unless the reply names a
 * distinct originating channel — then it reads as that agent, which is how a
 * single endpoint produces a multi-voice transcript.
 *
 * `members` is the roster, so a named agent's face can be looked up rather
 * than left to fall back on its title-cased channel slug (issue #1185). The
 * host's own convention for that slug (`api/types.ts`'s note on `thread`) is
 * a desk id for a channel reply and a roster agent id for a direct message —
 * only the latter matches a `TeamMember.id`, so a miss here is expected for a
 * desk-originated cross-post and simply keeps today's name-seeded fallback,
 * never a wrong face.
 */
export function senderOf(
  m: ChatMessage,
  channel: Channel,
  members: TeamMember[],
  youAvatar?: string,
): Sender {
  // Still "You" rather than your name: in your own transcript the second person
  // is what identifies the line, and a name there would read as somebody else.
  // Only the face is yours — which is the half a reader scanning a busy channel
  // actually picks their own lines out by.
  if (m.from === "you") return { key: "you", name: "You", kind: "you", avatar: youAvatar };
  if (m.from === "system") return { key: "system", name: "System", kind: "system" };

  const named = m.channel?.trim().toLowerCase() ?? "";
  if (named && !COMPANY_VOICE.has(named)) {
    const agent = members.find((mem) => mem.id === named);
    return {
      key: `agent:${named}`,
      name: titleize(m.channel ?? ""),
      kind: "agent",
      tone: named,
      avatar: agent?.avatar,
      agentId: agent?.id,
    };
  }

  // A desk speaks as itself and wears its own tone; only the main line — the
  // one channel with no tone of its own — speaks as the company. A DM's
  // "channel" is the teammate on the other end, so its avatar is theirs.
  return {
    key: `channel:${channel.id}`,
    name: channel.voice ?? channel.name,
    kind: channel.kind === "dm" || channel.tone ? "agent" : "company",
    tone: channel.tone,
    avatar: channel.member?.avatar,
    // A DM's other end is a roster teammate; a desk channel's voice is the desk
    // itself, which has no profile of its own to open.
    agentId: channel.member?.id,
  };
}

function titleize(s: string): string {
  return s.replace(/[._-]+/g, " ").replace(/\w\S*/g, (w) => w.charAt(0).toUpperCase() + w.slice(1));
}

export const initials = nameInitials;

/* ---- timeline grouping ---- */

/** Consecutive lines from one sender inside this window collapse into a run. */
const GROUP_WINDOW_MS = 5 * 60 * 1000;

export interface TimelineEntry {
  message: ChatMessage;
  sender: Sender;
  /** True when this row continues the run above it — no avatar, no name. */
  continuation: boolean;
  /** Set on the first row of a new calendar day; the divider label. */
  dayLabel?: string;
  /** Replies hanging off this row, oldest first. */
  replies: ChatMessage[];
  /**
   * The distinct voices in those replies, in the order they first spoke
   * (issue #1324).
   *
   * Resolved here rather than in the row because resolving a sender needs the
   * channel and the roster, and neither reaches the renderer. Without it the
   * summary row could only seed a face on `message.channel` — one value shared
   * by every reply in a thread — so a three-face pile drew one colour three
   * times and said nothing at all.
   *
   * Deduped by `Sender.key`: a pile is a list of *people*, and someone who
   * replied four times is still one face.
   */
  replySenders: Sender[];
}

/**
 * The distinct voices in a run of messages, in first-spoken order.
 *
 * Goes through the same {@link senderOf} every rendered row does, so a face in
 * a thread's summary pile is the same face that thread shows when it is opened.
 * A system line is dropped: it has no voice to draw, and a pile that counted it
 * would claim one more participant than the thread has.
 */
function distinctSenders(
  messages: ChatMessage[],
  channel: Channel,
  members: TeamMember[],
  youAvatar?: string,
): Sender[] {
  const byKey = new Map<string, Sender>();
  for (const m of messages) {
    if (m.from === "system") continue;
    const sender = senderOf(m, channel, members, youAvatar);
    if (!byKey.has(sender.key)) byKey.set(sender.key, sender);
  }
  return [...byKey.values()];
}

/**
 * Flatten a channel's messages into rows the timeline can render directly.
 *
 * Replies are folded into their parent rather than laid out inline: a parent
 * carries its own replies and renders a summary row, matching how a threaded
 * chat keeps the main channel readable.
 */
export function buildTimeline(
  messages: ChatMessage[],
  channel: Channel,
  members: TeamMember[],
  /** Your own face, so your lines in a busy channel are yours at a glance. */
  youAvatar?: string,
): TimelineEntry[] {
  const replies = new Map<string, ChatMessage[]>();
  for (const m of messages) {
    if (!m.parentId) continue;
    const bucket = replies.get(m.parentId);
    if (bucket) bucket.push(m);
    else replies.set(m.parentId, [m]);
  }

  const entries: TimelineEntry[] = [];
  let prev: TimelineEntry | undefined;

  for (const m of messages) {
    if (m.parentId) continue;
    const sender = senderOf(m, channel, members, youAvatar);
    const newDay = !prev || !sameDay(prev.message.at, m.at);
    const continuation =
      !newDay &&
      !!prev &&
      prev.sender.key === sender.key &&
      sender.kind !== "system" &&
      m.at - prev.message.at < GROUP_WINDOW_MS &&
      // A row with replies ends its run — the summary row below it would
      // otherwise sit between two lines that read as one utterance.
      prev.replies.length === 0 &&
      // Who *typed* it ends a run too (issue #1734, codex review of #1740).
      //
      // A collaborator's message and an agent reply share a sender key: both
      // are `from: "company"`, and the offline echo brain names its outbound
      // channel `operator` exactly as an operator message does, so `senderOf`
      // resolves both to the channel's own voice. Grouped, the second row is a
      // continuation — no author line, and therefore no Placeholder marker —
      // and it reads as part of the first one's utterance. That runs both ways
      // and is wrong both ways: an echo reply hides inside a colleague's run
      // unmarked, and a colleague's own words sit under an author line the
      // marker has already labelled as the echo brain's.
      //
      // Breaking the run is the honest rendering rather than a workaround: two
      // consecutive lines with different authorship are not one utterance, and
      // a run is a claim that they are.
      !!prev.message.byPerson === !!m.byPerson;

    const own = replies.get(m.id) ?? [];
    const entry: TimelineEntry = {
      message: m,
      sender,
      continuation,
      dayLabel: newDay ? formatDay(m.at) : undefined,
      replies: own,
      replySenders: distinctSenders(own, channel, members, youAvatar),
    };
    entries.push(entry);
    prev = entry;
  }

  return entries;
}

/**
 * An approval this console has watched being decided (#379).
 *
 * The **summary is kept, not just the verdict**, and that is the whole point:
 * the host drops a resolved approval from `GET …/approvals` immediately, so a
 * console holding only the verdict has nothing left to draw and the card blinks
 * out of the thread the moment it is decided — which reads as the request
 * having been lost, not answered. Keeping the last-seen summary is what lets it
 * settle in place instead.
 */
export interface DecidedApproval {
  verdict: Verdict;
  approval: ApprovalSummary;
}

/**
 * One row of a channel, which is no longer only a message (#379).
 *
 * A parked approval is a **distinct kind**, not a synthetic `ChatMessage`. It
 * has to be: a card is decidable, carries live server state, and settles into a
 * terminal state — none of which a message row can represent, and faking one
 * would mean inventing an id, a sender and a body for something that is not an
 * utterance. Keeping it separate is also what lets the card *derive* from
 * `feed.approvals` rather than being appended once and then going stale.
 *
 * Both kinds carry `at` so the two streams interleave on real time, which is
 * the only ordering that reads correctly: the request appears where the
 * conversation was when it was raised.
 */
export type TimelineItem =
  | { kind: "message"; key: string; at: number; entry: TimelineEntry }
  | {
      kind: "approval";
      key: string;
      at: number;
      /**
       * Every gated call the same turn parked, oldest first (#842).
       *
       * Usually one. A research turn that reaches three sites parks three, and
       * the conversation asks about them **once** — one card listing three
       * hosts — rather than interrupting the operator three times for one piece
       * of work. Each entry stays its own approval underneath: its own id, its
       * own decision, its own host-scoped grant on approve.
       *
       * Never empty. {@link buildTimelineItems} only mints an item when it has
       * an approval to put in it, so a renderer can read `approvals[0]` for the
       * facts the whole batch shares (the asker, the thread, the tool).
       */
      approvals: ApprovalSummary[];
      /**
       * The verdicts this console has witnessed, keyed by approval id (#842).
       *
       * A decided approval leaves `feed.approvals` on the next refresh, so
       * without this the card would simply vanish mid-glance — an abrupt
       * unmount that reads as the request having been lost. Holding the
       * witnessed verdict lets it settle into a terminal state instead.
       *
       * Per item rather than per card, because a batch settles **item by
       * item**: the Approvals page decides one row at a time, and a card that
       * kept claiming three things were pending after one was approved there
       * would be the two surfaces drifting. A batch is fully settled only when
       * every id in {@link approvals} has an entry here.
       */
      decided: Record<string, Verdict>;
    };

/**
 * Interleave a channel's messages and the approvals raised in it, oldest first.
 *
 * `approvals` is expected to be pre-filtered to this channel by the caller —
 * the thread→channel mapping lives in the shell, which owns the desk list and
 * the roster, and this module stays pure.
 *
 * A `decided` card is kept even once the feed has dropped it, so the operator
 * sees their own decision land rather than the card disappearing.
 *
 * ## One card per turn (#842)
 *
 * Approvals sharing a `batch` — the host's key for the turn that parked them —
 * collapse into a single item. The conversation is interrupted once for one
 * piece of work, which is the whole of the issue; the grouping is presentation
 * only, and every approval inside the item is still decided on its own id.
 *
 * Approvals with **no** batch are never grouped, not even with each other. An
 * absent key means "the host did not say which turn this came from" — a
 * workflow node, a scheduler tick, an older host — and folding those together
 * would invent a batch out of two facts that are only alike in being unknown,
 * which is how an operator ends up approving something they were never shown.
 * Each gets its own card, exactly as before this existed.
 */
/**
 * The key that decides which approvals share one card (#842, #1891).
 *
 * The turn's `batch` when the host named one, and the approval's **own id**
 * otherwise — which is what makes "ungrouped" the safe default: an id is
 * unique, so a batchless approval can only ever group with itself. An absent
 * key means "the host did not say which turn this came from" (a workflow node,
 * a scheduler tick, an older host), and folding those together would invent a
 * batch out of two facts that are only alike in being unknown, which is how an
 * operator ends up approving something they were never shown.
 *
 * Extracted so the board card groups exactly as the transcript does (#1895
 * review). It rendered a paused card's whole queue as one `ApprovalRow`, so a
 * card holding two turns' parks — or several batchless ones — offered a single
 * Approve that authorised across them. The rule was already written down here;
 * the second surface just wasn't reading it.
 */
export function approvalBatchKey(approval: ApprovalSummary): string {
  return approval.batch ?? `solo:${approval.id}`;
}

export function buildTimelineItems(
  entries: TimelineEntry[],
  approvals: ApprovalSummary[],
  decided: Record<string, DecidedApproval> = {},
): TimelineItem[] {
  const items: TimelineItem[] = entries.map((entry) => ({
    kind: "message" as const,
    key: entry.message.id,
    at: entry.message.at,
    entry,
  }));

  // Insertion-ordered, so a batch lands where its **first** approval did rather
  // than wherever the last one happened to arrive. The caller hands us the
  // pending feed followed by the settled ones, so an item decided on the
  // Approvals page rejoins the card it was raised in instead of opening a
  // second one below it.
  const batches = new Map<string, ApprovalSummary[]>();
  for (const approval of approvals) {
    const key = approvalBatchKey(approval);
    const bucket = batches.get(key);
    if (bucket) bucket.push(approval);
    else batches.set(key, [approval]);
  }

  for (const [key, batch] of batches) {
    batch.sort((a, b) => a.at_millis - b.at_millis || (a.id < b.id ? -1 : a.id > b.id ? 1 : 0));
    const verdicts: Record<string, Verdict> = {};
    for (const approval of batch) {
      const verdict = decided[approval.id]?.verdict;
      if (verdict) verdicts[approval.id] = verdict;
    }
    items.push({
      kind: "approval",
      key: `approval:${key}`,
      // The turn asked once, at the moment its first call was gated. Placing
      // the card at the earliest of the batch is what keeps it beside the
      // message that provoked it.
      at: batch[0].at_millis,
      approvals: batch,
      decided: verdicts,
    });
  }

  // Stable within a millisecond: a card raised by the very turn whose reply
  // shares its timestamp should sit after that reply, not shuffle between
  // renders. `sort` is stable in every engine this ships to, so equal `at`
  // keeps insertion order — messages first, then cards.
  return items.sort((a, b) => a.at - b.at);
}

/* ---- formatting ---- */

export function formatTime(at: number): string {
  return new Date(at).toLocaleTimeString(undefined, { hour: "numeric", minute: "2-digit" });
}

export function sameDay(a: number, b: number): boolean {
  return new Date(a).toDateString() === new Date(b).toDateString();
}

export function formatDay(at: number): string {
  const d = new Date(at);
  const today = new Date();
  const yesterday = new Date(today);
  yesterday.setDate(today.getDate() - 1);
  if (d.toDateString() === today.toDateString()) return "Today";
  if (d.toDateString() === yesterday.toDateString()) return "Yesterday";
  return d.toLocaleDateString(undefined, { weekday: "long", month: "long", day: "numeric" });
}

/* ---- reactions ---- */

/** The palette the hover bar offers, in the order it offers them. */
export const QUICK_REACTIONS = ["👍", "🎉", "👀", "✅", "❤️"] as const;

/**
 * Toggle the reader's own reaction, leaving everyone else's alone (issue #364).
 *
 * Reactions are per-person rows now, so a toggle adds or removes exactly one —
 * the reader's. It used to replace a count, which meant tapping an emoji
 * somebody else had already used silently wiped their reaction.
 *
 * `label` is how the reader will be shown in the chip's tooltip until the host
 * says otherwise. Returns `undefined` for an empty result so a message with no
 * reactions carries no key at all.
 */
export function toggleReaction(
  reactions: Reaction[] | undefined,
  emoji: string,
  label: string,
): Reaction[] | undefined {
  const rows = reactions ?? [];
  const mine = rows.some((r) => r.emoji === emoji && r.mine);
  const next = mine
    ? rows.filter((r) => !(r.emoji === emoji && r.mine))
    : [...rows, { emoji, by: label, mine: true }];
  return next.length ? next : undefined;
}

/** Whether the reader has already reacted to a message with this emoji. */
export function hasReacted(reactions: Reaction[] | undefined, emoji: string): boolean {
  return !!reactions?.some((r) => r.emoji === emoji && r.mine);
}

/** One emoji's chip: its rows collapsed into a count and a who-list. */
export interface ReactionChip {
  emoji: string;
  count: number;
  /** Whether one of the rows is the reader's. */
  mine: boolean;
  /** Everyone who reacted with it, in the order the host listed them. */
  by: string[];
}

/**
 * Group per-person reaction rows into the chips the row renders.
 *
 * Chips keep first-reacted order rather than sorting by count, so a message's
 * reactions do not reshuffle under the reader as others react.
 */
export function reactionChips(reactions: Reaction[] | undefined): ReactionChip[] {
  const chips: ReactionChip[] = [];
  const byEmoji = new Map<string, ReactionChip>();
  for (const row of reactions ?? []) {
    let chip = byEmoji.get(row.emoji);
    if (!chip) {
      chip = { emoji: row.emoji, count: 0, mine: false, by: [] };
      byEmoji.set(row.emoji, chip);
      chips.push(chip);
    }
    chip.count += 1;
    chip.mine ||= row.mine;
    chip.by.push(row.by);
  }
  return chips;
}

/**
 * Drop a dismissed card from **every** channel's transcript (issue #984).
 *
 * The channel-level counterpart of {@link clearTaskCard}, and it exists for the
 * same reason one level up. That helper keys on the card rather than the clicked
 * row because one card can be named by several lines; this one keys on the card
 * rather than the active channel because those lines can sit in several
 * *channels* — a dispatch marker lands in the origin thread's channel, not
 * necessarily the one the operator is looking at. Clearing only the active
 * channel leaves the rest linking to a card the host no longer has.
 *
 * Returns the same object when nothing changed, so React sees no new state.
 */
export function clearTaskCardEverywhere(transcripts: Transcripts, taskId: string): Transcripts {
  let changed = false;
  const next: Transcripts = {};
  for (const [channelId, messages] of Object.entries(transcripts)) {
    const cleared = clearTaskCard(messages, taskId);
    if (cleared !== messages) changed = true;
    next[channelId] = cleared;
  }
  return changed ? next : transcripts;
}
