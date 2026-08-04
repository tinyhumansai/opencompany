# Chat — the workspace

`#/chat` is a channel-and-DM workspace: a channel rail, a threaded timeline, a
composer, an optional thread panel, and an optional member pane. It replaces
three older surfaces — the Conversation page's chat list, the Team page, and
the desks the two shared without ever being connected.

## Routing

`#/chat/<channelId>` — the channel id is the hash's second segment, so a
channel is linkable and survives a refresh. An unknown id falls back to
`general` rather than erroring.

- Desks are `#general`, `#strategy`, `#creative`, `#front-desk` (`lib/desks.ts`).
- A DM is `dm:<slug-of-name>` — e.g. `#/chat/dm:designer`.

DM ids key on the teammate's **name**, not their roster id: the starter roster
mints ids from a module counter (`lib/team.ts`), so those differ between two
calls in the same session and a saved link would point at the wrong person.

## What is real and what is console-local

Every channel and DM posts to the **same** company chat endpoint. A channel
scopes a transcript and fixes the company side's identity; it is not a separate
backend, and there is no per-channel routing on the host.

Console-local, because the host has no surface for them yet:

| | |
|---|---|
| Threads | A reply carries `parentId` and stays out of the channel timeline. |
| Reactions | Toggled on the message in memory; not persisted anywhere. |
| Unread counts | The rail renders them, but nothing ever arrives in a channel you are not looking at — every reply answers a line you just sent. `ChatView` passes `{}`. |

Transcripts are per-session memory. Closing the tab drops them.

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
| `MembersPane.tsx` | The roster — what the Team page used to be. |
| `AddMemberDialog.tsx` | Define a teammate. |

`../ChatView.tsx` owns the state and composes them.

## Grouping rules

Consecutive lines from one sender within five minutes collapse into a run: the
first row carries the avatar and the author, and continuation rows leave the
gutter empty and reveal their timestamp there on hover. A run also breaks on a
day boundary, and on any row that has replies — a summary row between two lines
that read as one utterance is worse than an extra avatar.
