# One agent, one session — and talking as a tool call

Two changes that only make sense together: an agent that is continuous, and an
agent that speaks by calling a tool.

## Before

One `oh::agent::Agent` is held per `(company, agent_id)`. Its in-memory history
was **cleared and re-seeded from the incoming desk's transcript every time the
chat changed** (`bound_chat`, `src/harness/built_in/mod.rs`). That kept two
conversations from bleeding into each other, and it meant an agent had no
continuous existence: it could not notice that the question just asked in a DM
is the one it answered on a desk an hour ago, because between the two it had
been emptied.

Speaking was not a tool either. Every other thing an agent can do is one — it
writes a ledger row with `record_entry`, opens a card with `spawn_task` — but a
turn's **return text** was the message, journaled on its behalf. So it could not
choose a recipient, could not say something to one teammate rather than to the
room, and could not decline to speak; the only way to stay quiet was to return
an empty string, which reads as a failed turn.

## The session

`src/harness/built_in/agent_session.rs`. The session is never cleared. Each turn
the agent is handed the rows it has **not yet seen**, across **every** channel it
can read, each cued with where it was said:

```text
While you were away, this was said elsewhere in the company. It is context,
not a request — answer the message at the end of this turn.

[#Brand · copy] Tone should be warmer.
[dm · ceo] What did design decide?
```

`AgentSessionState` is a **watermark**, modelled on
`tinyhivemind::sharing::SharingState` minus its `conversation` field — that field
is what makes the vendored type return `ReinitializeReason::ConversationChanged`
on a channel switch, which is the exact behaviour being removed.

There is no mailbox and no per-agent queue, because `tinyhivemind` refuses to
have one: `NoDispatchReason::SelfMention`, `NoReferralReason::SelfMention` and
`UtteranceRejection::SelfRecipient` all decline self-addressing by name. The
architecture is **stigmergic** — work leaves an attributed trace in a shared,
globally sequenced log, and the trace is the stimulus for the next turn.
Continuity is a watermark the host owns, not a message an agent posts to itself.

### What is given up, and what is not

| | |
|---|---|
| **Channel isolation** | Given up, deliberately. This is what `#1725` / `#1730` / `#1890` installed, and the replacement is the cue: every delivered row is prefixed with its channel, per line, through the same `prefix_every_line` machinery that already defends attribution against forgery. |
| **Audience isolation** | **Kept.** A private aside this agent is not party to still never reaches it. `readable_by` applies the same narrowing `EpisodeDriver` applies with `Viewer::Agent`. |
| **A bound on growth** | Kept, and stated: `SESSION_DELTA_LIMIT` (60 rows) and `SESSION_SCAN_LIMIT` (2048 raw rows). Crossing either is a `Reinitialize`, which falls back to the recent-window seed — the honest answer when a partial replay would be worse context than a fresh window. |

The ordering is a contract: **commit `next_state` only after the agent has
accepted the rows.** Committing first would let a failed turn leave the session
believing it read something it never saw.

## Talking as a tool call

`src/harness/speech_tools.rs`. Off unless the manifest says so:

```toml
[speech]
enabled = true
```

**Company-level, not per-desk.** `[group_chat.hive.aside]` and
`[group_chat.hive.referral]` nest under a desk because they are properties of a
deliberation on that desk. Speech is a property of an agent's *session*, which
spans every desk it sits on plus its DM plus the General line. A per-desk knob
would let one agent speak by tool call on one desk and by return text on another
inside one unbroken session.

The names, argument shapes and description text all come from
`tinyhivemind::speech`, which states them once as data and asks a host to render
them **verbatim**. Nothing here invents a contract.

| Tool | Effect |
|---|---|
| `desk_post { message, desk? }` | Say one thing to a channel — the one being answered in, or any channel this agent sits on. Collected, not appended — see below. |
| `desk_dm { to, message }` | Leave one thing for named teammates, one row in each of *their* channels. |
| `desk_close { message }` | Say one last thing and report the work finished. |
| `desk_read { limit }` | Read further back in this channel than the turn was handed. Clamped by the crate's own `READ_MAX`. |

The names are prefixed because the crate's are bare and it anticipates exactly
this: *"an MCP server called `desk` serving `post` presents it as `desk_post`,
and the descriptions are written to read correctly either way."* `read` and
`post` unqualified would sit beside `read_thread`, `read_ledger` and
`pages_read` with no way for a model to pick.

### The host appends

A post does **not** append. The crate's own rule is that *"a tool call is a
request to speak — the host appends, the host decides"*, and the host that
appends is the reply path that has always appended: it carries the folded steps,
the live SSE frame, the resolved mentions and the board-card correlation, none of
which a tool holds. So `desk_post` and `desk_close` are collected in
`delegation::TurnSpeech` and become the turn's reply.

`desk_dm` is the exception and journals directly, because a row addressed to
somebody else's channel is not something this turn's single reply can express.
So is a `desk_post` that names a `desk` other than the one being answered in:
the turn's reply belongs to the conversation the turn is in, so a line meant for
elsewhere has to be its own row.

### A DM goes to the recipient's channel

One row per recipient, `chat_id` set to that teammate's own DM key, `audience`
empty.

It was built the other way first — one row in the **ambient** channel with
`audience: [recipient]` — because that needed no journal migration, and a live
run showed what it cost. Which channels reach an agent is decided by
`chat_history::agent_channels`, and the speaker's own DM is never one of the
recipient's. So the row sat in the *operator's* DM with the speaker: the one
person named could not read it, the one person not named could, and the tool
reported success. The agent then told the operator the message had been
"passed to them directly (delivered)" — three times, for three messages that
reached nobody.

`audience` was the wrong field because it is the **asides** field, and an aside
is a narrowing *within a desk everybody named is already on*. A DM has no such
guarantee, so the narrowing has to be the channel itself. A non-empty `audience`
would also make `fold_asides` lift the row out of the transcript as a
deliberation aside, which it is not.

### A post can name a desk

`desk` is optional and defaults to the channel being answered in. A named one is
resolved against `agent_channels` — the same function that decides which
channels reach this agent's session — so an agent can speak exactly where it can
hear, and nowhere else.

A desk it does not sit on is refused, and the refusal names what it can reach.
Reaching another desk is a **referral**: a crossing the library already models,
with its own provenance chip and its own return path. Letting `desk_post` write
into a room its author is not in would put a line in front of people with no
record of who let it in.

### It never silences a turn

An agent that answers **without** calling a speech tool still has its return text
journaled, exactly as before. Going quiet because a model forgot a tool call is
not an acceptable failure mode, so it is not one this knob can produce. The
suppression is gated on `TurnSpeech::spoke()`, not on the manifest flag.

### Nothing here starts a turn

`desk_dm` is one agent addressing another, so this is where the agent-to-agent
edge stops being hypothetical. It stays an edge that **journals a row and runs
nothing**: the recipient reads it on its next turn, through its own session
delta. The tool's result sentence says exactly that rather than reporting
delivery — an agent told "Said." will tell the person who asked that the message
was sent, which is how three undelivered messages were each reported as
delivered. That is the rule `CompanyEvent::AgentReply` already states about its
own `mentions`:

> never consulted by dispatch … an agent naming another agent draws a chip and
> files nothing to run. The edge does not exist, which is a stronger guarantee
> than an edge that is disabled.

and `mention_depth` beside it is the bound that would apply if it ever were. A
recipient hears about the row the next time it takes a turn, through its own
session delta — which is the stigmergic model, and needs no dispatch edge.

## Reading it back

`GET {scope}/agents/{agent_id}/session` — every row on every channel this agent
is **eligible to read**, in journal order, each stamped with its channel.
Projected through the **same** `chat_history::agent_channels` the live session
uses to decide what it may read, so the page never admits a channel the agent's
own session would be refused.

Eligible is not the same as *delivered*. A `desk_dm` "journals a row and runs
nothing" (above): the row exists the instant it is sent, but the recipient does
not actually receive it until its own next turn walks a session delta past the
watermark. This route has no access to that in-process watermark — it is
per-`AgentSessionState`, held by the live [`HarnessPool`](../../../src/harness/built_in/mod.rs)
under the `openhuman` feature, while this route compiles and answers in every
build. So a message queued behind another turn, or a fresh `desk_dm` the
recipient has not yet acted on, already appears here as an ordinary row —
this is the channel history the agent **may** read, not a record of what it
has **already** been handed. Treat a row here as "on the desk", not as "seen":
for the latter, cross-reference the raw-turns view of a turn that ran *after*
the row's timestamp.

The operator sees more than the agent does, deliberately: the route projects
with the caller's own `Viewer`, and `Audience::admits` admits every operator
unconditionally. Privacy between agents is a deliberation device, never a
security boundary — see [`hivemind-asides.md`](hivemind-asides.md).

The console renders it as the **Session** tab on `#/company/agent/<id>`
(`#/team/<id>` rewrites onto that address)
(`frontend/src/views/team/AgentSession.tsx`), reusing the room's own
`StepTimeline`, `ReferralConversation` and `AsideConversation` so an
agent-to-agent exchange reads the same way there as in the channel it happened
in.

### Raw turns

The same tab at `#/company/agent/<id>?tab=session&raw` renders the stream with
the chat rendering taken off: one block per journal row, in order, each stamped
`said` or `heard` — the latter shown in the literal `[channel · author] text`
shape [`render_cues`](../../../src/harness/built_in/agent_session.rs) prepends
to the turn, so what is on screen is the string the model was handed. Tool calls
unfold into their arguments and their result instead of collapsing into a step
chip, and referrals and asides print line by line rather than as a summary.

It renders the host's row rather than the mapped `ChatMessage`: `fromHistory`
resolves the speaker against the viewer, prefixes ids and lifts collapses onto
the bubble, all of which is what a reader asking for the raw turns is asking to
see past.

It is not a dump of the model's context window. That is process-local, bounded
by `max_history_messages`, and gone when the host restarts; the session an
operator can be shown is the one the host rebuilds from the journal on every
turn, and that is what this is.

`?raw` is an address rather than component state, for the reason `?edit` is —
"look at what it actually saw" is a link one operator sends another, and a link
that lands on the bubbles and asks the reader to find a switch has lost the
point of having been sent.

### The same view from the DM

A **Raw turns** control sits in the chat header of a DM
(`frontend/src/views/room/ChatHeader.tsx`), addressed as `#/chat/dm:<id>?raw`.
That is where the question gets asked — "why did it answer that" occurs to you
mid-conversation, and a control for it two navigations away is one nobody
finds.

It is offered only in a DM. A `#channel` has several agents and the Operator
feed has none, so the control would have to pick one for you, which is worse
than not offering it.

It shows **this conversation's** turns, filtered from the same per-agent route
by both DM spellings the host registers (the bare teammate id and `dm:<id>` —
see `chat_history::agent_channels`). A toggle changes how the thing in front of
you is drawn; it must not quietly change what the thing is, and flipping a DM
into a stream that also carries `#general` would do that. The pane links to the
cross-channel view for the operator who wants it.

Both surfaces render `frontend/src/views/room/RawTurns.tsx` — one component,
because "what the agent saw" is a claim about the runtime, and a claim that
reads differently depending on which screen you are on is two claims.
