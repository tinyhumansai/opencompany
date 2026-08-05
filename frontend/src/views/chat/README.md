# Chat — the workspace

`#/chat` is a channel-and-DM workspace: a channel rail, a threaded timeline, a
composer, an optional thread panel, and an optional member pane. It replaces
three older surfaces — the Conversation page's chat list, the Team page, and
the desks the two shared without ever being connected.

## Routing

`#/chat/<channelId>` — the channel id is the hash's second segment, so a
channel is linkable and survives a refresh.

- A desk's channel id is the host's desk id, which is also its chat thread id.
- A DM is `dm:<teammate-id>` — e.g. `#/chat/dm:designer` for a host roster
  agent, or `#/chat/dm:member-product-manager-1f3k` for a teammate this console
  invented.
- A host with no `.../desks` route falls back to `#general`, `#strategy`,
  `#creative`, `#front-desk` (`lib/desks.ts`).

Nothing resolves until `/desks` has answered — the view holds a loading state
rather than resolving against the fallback desks and swapping under you, which
is what made every deep link flash `#general` (issue #370). An id that doesn't
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

## What is real and what is console-local

Every channel and DM posts to the **same** company chat endpoint. A channel
scopes a transcript and fixes the company side's identity; it is not a separate
backend, and there is no per-channel routing on the host.

**Real — the host's, and reload-visible to every operator** (issue #364):

| | |
|---|---|
| Transcripts | Journaled by the host and rehydrated from `GET {scope}/chat/history` on load. This has been true since #65 — the "per-session memory" this file used to claim was already out of date. |
| Channel scoping | Every message carries the desk it was sent to, and the host filters server-side. Two people in two channels are not in the same room, and have not been since #53/#65. |
| Threads | A reply posts its `parent` — the parent message's own id — and comes back under it. Both halves of the exchange hang off the row the thread opened from. |
| Reactions | One durable row per person per emoji, so a chip says who reacted and whether one of them was you. `POST {scope}/chat/messages/{seq}/reactions` with an explicit `on`, which makes a retry idempotent. |
| Message ids | A sent message comes back with the id it was journaled under (`messageId`), which is what a thread reply or a reaction names. Until the id lands the row's reply/react actions are disabled and say why. |

**Still console-local:**

| | |
|---|---|
| Unread counts | Derived here from when this tab last looked at a channel — the host keeps no read receipts, so two consoles will disagree. The badge's tooltip says so. |
| A console-only teammate's other half | A starter-roster teammate is not on the company, so nothing answers their DM. The transcript is still saved; a notice above the composer says which half is missing. |

Reactions are deliberately **not** on the SSE feed: the frame would have to
carry the reacting person, and that stream has no per-viewer projection to turn
an actor into a label. They arrive on the next read.

Nothing was migrated. A message journaled before #364 loads with no parent and
no reactions, which is the truth about it. History journaled under the old
counter-minted `member-N` teammate ids stays orphaned — there is no honest
mapping from `member-3` to a person, and inventing one would be worse than the
loss.

## Membership

A desk's members come from the host (`GET {scope}/desks` → `members`, lead
first). They scope the header's count and the member pane, so a two-person desk
reads as two people rather than as the whole company (issue #369). A DM is a
two-person line: the header states 2, and the pane shows the teammate on the
other end.

The fallback desks in `lib/desks.ts` carry no membership — there is none to
carry — so a channel built from them falls back to the whole roster, and the
pane renders one plain list. The rest of the company is always one section
below, so adding a teammate or opening somebody's DM never needs a different
surface.

## Files

| | |
|---|---|
| `model.ts` | Channels, senders, timeline grouping, formatting. All pure. |
| `ChannelRail.tsx` | The channel/DM list, with collapsible sections. |
| `ChatHeader.tsx` | The bar above the timeline. |
| `MessageTimeline.tsx` | The scroll body: day dividers, channel intro, typing row. |
| `MessageRow.tsx` | One line — avatar gutter, author, body, reactions, hover action bar. |
| `MessageComposer.tsx` | The composer dock; also used compact in the thread panel. |
| `ThreadPanel.tsx` | Replies to one message, with their own composer. |
| `MembersPane.tsx` | Who is in this channel, then the rest of the roster. |
| `AddMemberDialog.tsx` | Define a teammate. |

`../ChatView.tsx` owns the state and composes them.

## Grouping rules

Consecutive lines from one sender within five minutes collapse into a run: the
first row carries the avatar and the author, and continuation rows leave the
gutter empty and reveal their timestamp there on hover. A run also breaks on a
day boundary, and on any row that has replies — a summary row between two lines
that read as one utterance is worse than an extra avatar.
