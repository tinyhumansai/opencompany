# Room — the workspace

The console calls this surface **Room**; the view id and every minted address
stay `chat`, because a view id is an address and renaming a row is not a reason
to break `#/chat/<channelId>` links (`docs/spec/runtime/console-sections.md`,
"Labels and view ids are allowed to differ"). The directory and the components
took the label; the URLs did not.

`#/chat` is a channel-and-DM workspace: a channel rail, a threaded timeline, a
composer, an optional thread panel, and an optional member pane. It replaces
three older surfaces — the Conversation page's chat list, the Team page, and
the desks the two shared without ever being connected.

## Routing

`#/chat/<channelId>` — the channel id is the hash's second segment, so a
channel is linkable and survives a refresh. The rail shows only direct messages
with at least one line, newest first; **New message** opens the full roster to
start an otherwise absent DM.

- A desk's channel id is the host's desk id, which is also its chat thread id.
- A DM is `dm:<teammate-id>` — e.g. `#/chat/dm:designer` for a host roster
  agent, or `#/chat/dm:member-product-manager-1f3k` for a teammate this console
  invented.
- There is no company-wide channel. A message that names no chat goes to the
  default agent's DM; the old `#general` line survives only as a read-only
  archive — see below.
- A host with no `.../desks` route falls back to `#strategy`, `#creative`,
  `#front-desk` (`lib/desks.ts`) under it.

Nothing resolves until `/desks` has answered — the view holds a loading state
rather than resolving against the fallback desks and swapping under you, which
is what made every deep link flash a fabricated channel (issue #370). An id that doesn't
resolve *after* they land opens the first channel and says so in a notice above
the timeline, so the URL and the content never disagree silently. A `/desks`
failure that isn't "this host has none" (404 or an empty list) is a retryable
error state, not invented channels.

DM ids key on the teammate's **id** (issue #364). A host roster agent has always
had a stable one; a console-invented teammate now does too — `lib/team.ts`
derives it from the role (or, for a hand-added teammate, the name) rather than
minting it from a counter. Keying on the id means renaming somebody does not
move their DM's URL or orphan the history already journaled under it.

Ids minted before #364 were `dm:<slug-of-name>-<hash>`. `resolveDmChannelId`
still resolves those for one release so a saved link lands, but nothing is ever
addressed or stored under one.

## `#general` — the read-only archive

The company-wide line is gone from the rail: `buildChannels` lists desks and
DMs only, and an unaddressed message lands in the default agent's DM. The host
still serves the old General history, so an explicit `#/chat/main` (or any
General spelling) opens `legacyGeneralChannel` — a `system` channel with no
composer, no reaction toolbar, no members pane and no empty-state cards. It is
never offered in the rail, and a bare `#/chat` does not restore onto it.

**The four spellings still fold.** The host journals the line under `""`,
`main`, `General` and `general` (`isGeneralChannel`, mirroring
`is_general_chat`), so `channelIdForThread`, the shell's thread → channel map
and the rehydration targets all resolve every spelling to the one place that
renders it, and old approvals raised there still link to it.

**Unless a blueprint desk claims the line.** A manifest `[[group_chat]]` with a
General id — or a General display name, since `resolve_desk_id` matches either
(`deskClaimsGeneralChannel`) — is grandfathered by the host: that desk keeps its
lead, its writes and its routing. It is an ordinary writable desk in the rail,
and `generalChannelId` sends every General spelling to it instead of the
archive. Operator-created overlay desks never claim the line.

**A teammate whose id is a General spelling keeps its DM under `dm:<id>`.** The
bare key folds onto the line, so `dmThreadId` addresses that one DM prefixed,
and `channelIdForThread` resolves desk, then the General fold, then the roster.

**Desk affordances follow the desk list.** The lead badge and the "Manage on the
org chart" link are gated on `activeIsDesk` — whether `GET .../desks` holds the
active id — so the archive gets neither, and a grandfathered desk keeps both.

## What is real and what is console-local

Every channel and DM posts to the **same** company chat endpoint. A channel
scopes a transcript and fixes the company side's identity; it is not a separate
backend, and there is no per-channel routing on the host.

**Real — the host's, and reload-visible to every operator** (issue #364):

| | |
|---|---|
| Transcripts | Journaled by the host and rehydrated from `GET {scope}/chat/history` on load. This has been true since #65 — the "per-session memory" this file used to claim was already out of date. The rehydration is *not* instant, and the timeline has to say so: see [Empty, or not answered yet](#empty-or-not-answered-yet). |
| Channel scoping | Every message carries the desk it was sent to, and the host filters server-side. Two people in two channels are not in the same room, and have not been since #53/#65. |
| Threads | A reply posts its `parent` — the parent message's own id — and comes back under it. Both halves of the exchange hang off the row the thread opened from. |
| Reactions | One durable row per person per emoji, so a chip says who reacted and whether one of them was you. `POST {scope}/chat/messages/{seq}/reactions` with an explicit `on`, which makes a retry idempotent. |
| Message ids | A sent message comes back with the id it was journaled under (`messageId`), which is what a thread reply or a reaction names. Until the id lands the row's reply/react actions are disabled and say why. |
| Message intent | The composer's three positions — "Just chatting" / "Do it once" / "Build me the workflow" — travel as `deliverable` on the message and are journaled with it. None starts selected: an unmarked line leaves no operator override, so the host's triage decides whether to open a card. `chat` withholds the card the host would otherwise open by construction; it does **not** take the orchestrator's own `spawn_task` away, so it means "not automatically carded", not "never carded". |

**Still console-local:**

| | |
|---|---|
| Unread counts | Derived here from when this tab last looked at a channel — the host keeps no read receipts, so two consoles will disagree. The badge's tooltip says so. |
| A console-only teammate's other half | A starter-roster teammate is not on the company, so nothing answers their DM. The transcript is still saved; a notice above the composer says which half is missing. |
| Channel rail density | The desktop channel rail can collapse to an icon strip, preserving channel reachability while giving the transcript back its width. This is stored per browser connection and company; it is not a company-wide shell setting and does not change the full rail below `lg` (issue #1340). |

Reactions are deliberately **not** on the SSE feed: the frame would have to
carry the reacting person, and that stream has no per-viewer projection to turn
an actor into a label. They arrive on the next read.

Nothing was migrated. A message journaled before #364 loads with no parent and
no reactions, which is the truth about it. History journaled under the old
counter-minted `member-N` teammate ids stays orphaned — there is no honest
mapping from `member-3` to a person, and inventing one would be worse than the
loss.

### A read-only channel offers no new reaction (#1986)

Reacting writes into the company's transcript exactly as sending does,
and the host authorizes it through the very same gate (`chat_actor`,
`src/server/operator.rs`, whose own doc says reacting "can be neither easier nor
harder than saying something"), so a surface that states *nothing can be
posted here* must not offer it either.

`MessageTimeline` reads `channel.system` — the flag `RoomView` derives its own
`readOnly` from, and the one the channel intro already gates on — and
`MessageRow` then **removes** the hover toolbar's five quick reactions rather
than disabling them. The same answer #1984 gave the composer, for the same
reason: a greyed-out control is still a claim that the action exists, and this
strip is revealed by CSS alone (`group-hover/message:flex`), so leaving the
buttons mounted leaves them reachable by pointer, by keyboard focus and by a
screen reader.

Reactions **already** on such a line still render, disabled, with a tooltip
saying why. One somebody left is content and this channel is the only record
of it; hiding it would lose information rather than withdraw an offer. Only the
ability to add or toggle one goes. The way into a thread stays too — an
archived message is still worth reading the replies under, and what may be
*written* in one is `ThreadPanel`'s question, answered there.

**This one is UX, not enforcement.** The reaction route
(`POST {scope}/chat/messages/{seq}/reactions`) is addressed by sequence number
and runs no read-only check, so a reaction on an archived row is still accepted
from anyone who can issue the request.

## Empty, or not answered yet

`Transcripts` is `Record<string, ChatMessage[]>`, and the timeline reads it as
`transcripts[channel.id] ?? []`. That `??` collapses two different facts into
one: *nobody has asked the host about this channel* and *the host says this
channel has nothing*. The channel intro then printed the second — "This is the
start of your direct message with …" — while the first was true, so reloading a
DM with months of history rendered it as brand new for as long as the fetch took
(issue #934). Nothing was lost; it just read exactly like it had been.

`HistoryHydration` in `timeline.ts` is the missing fact, and `historyReady()` is
the one place that reads it:

| State | May the intro claim "this is the start"? |
|---|---|
| `byChannel[id] === "loading"` | No — the request is in flight. |
| `byChannel[id] === "ready"` | Yes. Settled, including a host that answered with nothing or failed outright. |
| No entry, `discovered === false` | No. The shell's pass has not reached this channel yet — `RoomView` resolves its own desk list independently, so it can paint a channel a moment before the shell marks it. |
| No entry, `discovered === true` | Yes. The pass ran and did not claim it, so nothing is coming — a console-only teammate, or a host with no `chat/history`. Holding a spinner forever would be a worse lie than the one this prevents. |

`AppShell` owns the map because it owns the fetches. A channel is marked
`loading` *before* its request goes out, never after — the gap between "this
channel exists" and "its history is in flight" is precisely the window the bug
lived in.

While a channel is pending and has no rows, `MessageTimeline` renders skeleton
rows in place of the claim. The avatar and title above them still render: those
state where you are, which is not a claim about history. Rows that arrived
locally — a message sent before hydration landed — render immediately; only the
assertion of emptiness waits.

## Membership

A desk's members come from the host (`GET {scope}/desks` → `members`, lead
first). They scope the header's count and the member pane, so a two-person desk
reads as two people rather than as the whole company (issue #369). A DM is a
two-person line: the header states 2, and the pane shows the teammate on the
other end.

The fallback desks in `lib/desks.ts` carry no membership — there is none to
carry — so a channel built from them falls back to the whole roster, and the
pane renders one plain list. The `#general` archive states the whole roster as
its membership, derived, but offers no pane. The rest of the company is always one section
below, so adding a teammate or opening somebody's DM never needs a different
surface.

### Editing it is somewhere else, on purpose (#485)

The pane **links** to the org chart — "Manage on the org chart" beside "In this
channel", "Staff it on the org chart" when the desk is empty — and grows no
membership controls of its own. The link opens `#/company/<deskId>`, which the
chart scrolls to and focuses. A desk's channel id *is* its desk id
(`deskFromDto`), so nothing maps between them.

The reason is the pane's own rule. `channelMembers` **drops** a member id that
resolves to no roster teammate: you cannot message nobody, so no row is drawn
for one. The chart does the opposite and **badges** it "Not on the roster". A
membership editor has to show every seat, and that ghost seat is exactly the one
an operator most needs to remove — so an editor here would either break the drop
rule this pane is built on or be unable to remove ghosts. Editing lives where
ghosts are visible. Keep the two behaviours as they are; the divergence is what
makes the split coherent rather than arbitrary.

The link is offered only for a **host-backed desk channel**. A DM is not a desk,
a fallback desk names one the host does not have, and the `#general` archive is
not a desk at all — the chart would open on nothing in all three cases, so none of
them gets a link. The test is `activeIsDesk`: whether the desk list holds the
active id. Asked of the list rather than of the id's spelling, so a blueprint
desk grandfathered under a General id — which the host does list and the chart
does hold — keeps its link instead of losing it to a name match. There is no
admin gate, because the chart has none either (its controls are gated by
provenance).

The channel rail stays flat, and #485 settled that too: it already is the org
chart's desk level, since no desk can name a parent desk. See
`views/company/README.md`.

## Files

| | |
|---|---|
| `channels.ts` | What a channel is: desks, DMs, the `#general` archive, and the id grammar. Pure. |
| `timeline.ts` | Senders, hydration, grouping, the timeline items (messages, approvals, rounds, completion markers), reactions. Pure. |
| `review.ts` | A card's lifecycle inside a conversation: the settle pill a verdict hangs off, and the budget-pause markers. Pure. |
| `model.ts` | A barrel re-exporting the three above, so one import address still reaches all of it. Declares nothing. |
| `RoundBand.tsx` | One round of a desk answering as a room: the seats that ran together, each lane's live state, and the rows they produced (`data-testid="round-band"`, `data-round-status`). |
| `EpisodeCompleteMarker.tsx` | The centred pill that says an episode is over — how many rounds, who closed it, and whether the host cut it off. |
| `ChannelRail.tsx` | The channel/DM list, with collapsible sections. |
| `ChatHeader.tsx` | The bar above the timeline. |
| `MessageTimeline.tsx` | The scroll body: day dividers, channel intro, loading skeleton, typing row. |
| `MessageRow.tsx` | One line — avatar gutter, author, body, reactions, hover action bar, the board-card chip (link plus its dismissal, issue #984), and the utterance chip on a row an episode committed (`components/episode/UtteranceChip`). |
| `MessageComposer.tsx` | The composer dock; also used compact in the thread panel. |
| `ThreadPanel.tsx` | Replies to one message, with their own composer. |
| `bottomAnchor.ts` | How close to the bottom still counts as the bottom. Pure. |
| `useBottomAnchor.ts` | The four rules that keep a transcript on its newest row — arrival, growth, scroller resize, content resize — plus whether it is still parked there. Used by both panes above. |
| `JumpToLatest.tsx` | The control offered while the reader has scrolled away; a sibling of the scroller, never a child. |
| `MembersPane.tsx` | Who is in this channel, then the rest of the roster. |
| `AddMemberDialog.tsx` | Define a teammate. |

`../RoomView.tsx` owns the state and composes them.

## Rounds and episodes

A desk of two or more answers as a room, and the transcript shows it as a
**grouping strip, not a page**. The seats' replies are ordinary rows; what the
band adds is that they were written *together*: `lib/episodes.ts` folds the
rows carrying `episode: {id, revision, kind}` with the live frames the shell
holds (`lib/episode-frames.ts`, fed by `episode_opened`, `round_started`, the
turn bracket, `round_committed`, `dm_delivered`, `episode_completed`), and
`timeline.ts` collapses each round's rows into one `round` item at the position
of its first row — a round no row has reached yet takes the moment it opened —
with an `episode_complete` item after the last round. After a reload the frames
are empty and every completed episode is rebuilt from `chat/history` alone; the
frames only ever add the present tense (a lane still working, a seat that timed
out). A DM, `#general` and a one-seat desk carry no `episode` and render exactly
as before: the affordance follows the data, never the channel kind. The
styleguide's "Rounds" section renders every piece against a fixture.

## Grouping rules

Consecutive lines from one sender within five minutes collapse into a run: the
first row carries the avatar and the author, and continuation rows leave the
gutter empty and reveal their timestamp there on hover. A run also breaks on a
day boundary, and on any row that has replies — a summary row between two lines
that read as one utterance is worse than an extra avatar.

## One face per teammate

`TeammateAvatar` (`@/components/teammate-avatar`) draws its face from an
`avatar` **reference** — the one this teammate was given, or the mascot hashed
from its **id** when nobody has chosen (`TeamMember.avatar`, resolved once by
`fromDto` in `lib/team.ts`, see `docs/spec/runtime/avatars.md`). Seeding on the
id is why a rename never changes anyone's face (issue #1185), and carrying the
chosen face through the *same* field is why setting an icon changes it
everywhere at once rather than on the page it was set from. It falls back to
hashing the `name` it is given only when no `avatar` prop is passed, which is
the honest answer for a voice with no roster entry behind it (a channel, a
cross-posted agent line `senderOf` couldn't match against the roster).

An uploaded face (`blob:<nodeId>`) is fetched through the authenticated client
rather than put straight in an `src` — the blob route needs a credential an
`<img>` cannot carry — so it arrives a render late and is cached module-wide.
That is the whole reason the tile keeps a tone-tinted square with initials
underneath: the gutter is never empty while an image is in flight, and a face
whose bytes were deleted degrades to a coloured tile rather than to a broken
image.

A DM is where seeding it wrong bites hardest: the rail row and `ChatHeader`
sit on screen together, and seeding them differently would put two faces on
one teammate — worse than the generic glyph the header drew before issue #1170.
Both go through `dmFace(channel)` in `channels.ts`, which reads
`channel.member.avatar`; a channel and a DM with no roster entry get `null`
there and wear a glyph (`#`, `Lock`, `CircleDot`) instead, because neither has
one person behind it. The header draws its tile at 24px, the floor below
which `TeammateAvatar`'s `markOnly` says a mascot is a smudge and the bare
tone tile is the honest mark.

Your own lines carry your own face too: `buildTimeline` takes a `youAvatar`,
which `RoomView` reads from the same `auth/me` call that resolves your role.
The name stays "You" — in your own transcript the second person is what
identifies the line, and your name there would read as somebody else — so only
the face is yours, which is the half you actually pick your lines out by.

**Every surface that resolves a sender needs it, not just the timeline**
(issue #1729). `ThreadPanel` resolves its own senders rather than reading
`TimelineEntry`, and it was passing three arguments to `senderOf` instead of
four — so a "you" line in a thread had no `avatar`, `TeammateAvatar` seeded on
the name it was given, and `avatarFor("You")` hashes to the same mascot the
agent happened to be wearing. Both participants drew one face and the thread
could not be read. The panel takes a `youAvatar` prop from `RoomView` for
exactly that reason; a new sender-resolving surface owes the same.

The main timeline's `senderOf(message, channel, members)` carries the same
seed for a message whose `channel` field names a distinct originating voice:
it looks that id up against the roster (`members.find`) the same way
`RoomView` already does elsewhere, and simply leaves the mascot unresolved —
falling back to the name seed, never a wrong face — when the id names a desk
rather than a teammate.

## A face is a way in

Clicking a teammate's face — in the gutter of a message, in the member pane, or
in a DM's header — opens `AgentProfileSheet`
(`@/components/agent-profile-sheet`): a right-hand panel with that agent's
persona, tier, desks and **resolved** tool grants, and two links out to their
own page (`#/team/<id>`, and `#/team/<id>?edit` for the page with its edit form
already open). The panel is mounted once by `AgentProfileProvider` in
`app-shell.tsx`, so no chat surface threads a client, a company scope or an open
flag of its own.

Only a voice that resolves to a roster teammate is clickable. `Sender.agentId`
is set exactly where `senderOf` **matched** the roster, never from the channel
slug that seeded the face — that slug is a desk id for a cross-posted line, and
a desk has no profile to open. `AgentAvatarButton` renders the bare avatar
rather than a dead button wherever there is no id behind it (a desk, the
company, you), which is also what it does outside the provider.

## One name per teammate

The header's title is `channel.name`; the muted slot past the divider is
`channelSubtitle(channel)`. A DM's `purpose` is the teammate's **description**,
falling back to their role — the field parallel to a desk's blurb, since both
answer "what is this line for". It used to be the role alone, which is an
identity field in a description slot, and `fromDto` resolves a teammate's name
as `dto.name?.trim() || dto.role`: a company that declares roles and names
nobody made the two slots one string, and every DM header in it read
`Backend Engineer │ Backend Engineer` (issue #1180).

So `channelSubtitle` returns `null` — not `""` — for a purpose that is empty or
that only repeats the title, compared case- and whitespace-insensitively. The
header drops the entire `<span>` when it does, divider included: the `border-l`
lives on that element, so keeping it empty would leave a rule hanging beside the
name. `ChannelRail`'s row tooltip and `MessageTimeline`'s conversation-intro
clause read the same helper, for the same reason. The rule is kind-agnostic: a
desk whose blurb just restates its slug is the identical non-fact under `#`, and
a blurb that says something the slug does not is untouched.
