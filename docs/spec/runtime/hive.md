# Hive desks

A desk with somebody to work **with** answers an operator message as a room:
the message opens an **episode**, the room runs it in **rounds** of concurrent
turns, seats talk to each other by posting, broadcasting and DMing, and the
episode ends when a seat reports the work complete.

That is the whole story for a `[[group_chat]]` of two or more members. A desk
of one, a DM, the General line and a workflow copilot thread run one ordinary
turn, exactly as they always did.

The mechanics come from [`tinyhivemind`](https://github.com/tinyhumansai/tinyhivemind)
(`vendor/tinyhivemind`) and its `tinyhivemind-openhuman` adapter, which binds
a desk to already-built `openhuman_embed::Agent`s and folds what the host
commits. OpenCompany hosts it: it owns the runtime, the agents, the journal,
the sequences, the scheduling and the tools. Nothing in the library appends a
row, waits, or runs a turn.

## The shape

```text
one process ── one openhuman_embed::Runtime
                 ├─ one openhuman_embed::Agent per (company, agent)     ← the seat
                 │     one stable session, one turn_lock
                 └─ one OpenHumanHive + CompletionDriver per [[group_chat]]   ← the room
                       members = cloned Agent handles (a shared agent is in every hive that lists it)
```

- **One runtime.** `openhuman_embed` allows one `Runtime` per process
  (`harness::openhuman_runtime::global`). It is booted at `serve` with
  `<data-dir>/openhuman` as its workspace and the TinyHumans key as its
  credential ([harnesses.md](harnesses.md)).
- **One agent per company agent.** `harness::build::agent_spec_for` renders a
  manifest `[[agent]]` into an `AgentSpec` — persona prompt, native tool scope,
  provider route, `Access::full()`, the `opencompany` MCP server, its skills
  dir and action dir — and `runtime.agent(spec)` mints the handle. The runtime
  id is `session_key::runtime_agent_id(company, agent)` (`{company}--{agent}`,
  lowercased, hashed past 64 chars). The handle is a cheap clone; the
  `CompanyAgent` beside it holds the agent's `turn_lock`.
- **One hive per desk.** `hive::graph::DeskHive` builds
  `HiveGraph::new(desk, candidates)` from the roster
  (`delegation_tools::tinyhivemind_desks` / `tinyhivemind_roster`) and binds
  every member with `AgentBinding::new(member_id, company_agent.agent.clone())`.
  Held in `HarnessPool.hives` and rebuilt with the roster and on
  `DeskCreated` / `DeskDeleted` / `DeskMembersChanged`.
- **A shared agent is one agent.** The CEO on both `engineering` and `content`
  is one `Agent` bound into two hives. Its `turn_lock` is what keeps that
  honest: it runs one turn at a time across every room it sits in, and two
  rooms' rounds otherwise proceed independently.

## Where a message goes

`hive::dispatch` is the chat body of `Brain::run_cycle` and the only gate:

| Surface (`ConversationRef.kind`) | What runs |
| --- | --- |
| `Direct` (`dm:<agent>` or a bare roster id) | one turn on that agent |
| `General` (`""`, `main`, `general`, `General`) | one turn on `delegation_tools::chat_responder` |
| `Workflow` (a copilot thread) | one **confined** turn (`confine.rs`); returns before this gate |
| `Desk`, one effective member | one ordinary turn; a bare reply is salvaged as a `post` and the episode auto-completes |
| `Desk`, two or more members | an **episode** |

`route_message` answers `Fallback` for the first three: no Jev, no driver, one
ordinary turn, journaled as `AgentReply`. `@mention` resolution is
`tinyhivemind_core::mention::{resolve, direct_responder, mentioned_members}` on
every surface.

Membership is `CompanyRecord::effective_desk_members` — overlay desks and
Team-API retirements count — so who is in the room and who the console says
is in it cannot drift.

## An episode

```text
operator message ─▶ route_desk (Jev | lead/mention fallback) ─▶ EpisodeOpened
      │
      ▼
 ┌─ pending_round ─▶ RoundStarted ─▶ tokio::spawn one turn per seat (own stack, turn_lock, timeout)
 │       ▼
 │  each seat ends its turn with exactly one speech call ─▶ AgentReply row (sequence = the commit)
 │       ▼
 │  apply_committed_round ─▶ RoundCommitted ─▶ HostAction::RunAgents → BroadcastRouted
 │                                            HostAction::DeliverDm  → DmDelivered
 └────── completion_status != Complete ◀──────┘
                 │
                 ▼ Complete
          EpisodeCompleted
```

**Opening.** The episode key is `(desk, thread_root ?? message_seq)`. A message
in a thread with an open episode resumes it (`driver.resume(state)` plus
`apply_assignment` for the mentioned or lead seat). Otherwise the host asks
`hive.route_desk(jev, None, &hive.desk_request(..), explicit_mention,
fallback = desk_lead)` for a `RoutingPlan`, journals `EpisodeOpened` with it,
builds `CompletionEpisodeState::opened(conversation, message_seq, members)`,
assigns the selected seats, and `driver.start`s.

**A round.** `driver.pending_round(&state)` names the seats with pending work,
at most `round_width` of them. The host journals `RoundStarted` and runs every
one **at the same time**: each seat's turn is its own `tokio::spawn`, waits on
that agent's `turn_lock`, and is bounded by `turn_timeout_secs` counted from
the moment it holds the lock — so a seat shared with a busy desk is delayed,
not timed out, by the other room. Turns in one round do not read each other;
they see the desk as it stood when the round opened.

**Exactly one action.** A seat's turn ends with exactly one speech tool call.
A turn that made none has its reply salvaged as a `post` when it reads as one
(`speech::fence::extract_post`); otherwise the seat is re-asked with a stricter
reminder, up to four times, and then the host journals `TurnFailed` and commits
a synthetic `complete_episode("(no action)")` for that seat so the room never
hangs on a silent member. A second speech call in one turn is refused by the
MCP server ("one action per turn; the first is recorded").

**Committing.** Each utterance is appended as an ordinary `AgentReply` row on
the desk — `parent` = the thread root, `episode: {id, revision, kind, to?,
routed_by?}`, `audience` = the recipients of a `dm` — and the row's `EventSeq`
**is** the `CommittedUtterance.sequence`. The host then folds the whole round
with `apply_committed_round(&state, &pending, committed, BroadcastRouting
{primary: jev, policy, roster_version, thread_context})`, journals
`RoundCommitted`, and persists the new `DriverState`. The library's actions are
descriptions; the host executes them: `RunAgents{agent_ids, plan}` schedules
the next round's seats and journals `BroadcastRouted{plan, router}`,
`DeliverDm{route, message}` journals `DmDelivered` and assigns the recipients.

**Ending.** When `completion_status` is `Complete` — every assigned seat has
called `complete_episode` — the host journals `EpisodeCompleted{reason:
complete_episode}`. The other reasons are `round_cap` (`max_rounds` spent),
`timeout`, `failed` (a journal append failed) and `membership_changed` (the
roster moved underneath the room). A completed episode is the transcript above
it; there is no closing summary row.

**Resuming.** `EpisodeStateSaved{episode_id, desk, thread_root, revision,
state, sharing}` is journaled after every commit (a ledger row, never
projected). On restart the host loads the latest row and replays every
`AgentReply` whose `episode.revision` is above it through `apply_committed`;
an exact replay is a no-op by the driver's receipts, so a crash between
`RoundCommitted` and the state save costs nothing twice.

**Concurrency.** Episodes of different desks are independent tasks tracked in
`HarnessPool.episodes`. There is no company-wide serial lock on chat and no
per-chat slot; the locks are the per-agent `turn_lock` and a per-episode lock.
The invariant the measurement (`opencompany measure`,
`scripts/measure-coordination.mjs`) checks: turns of different agents overlap
freely, across desks and within a round; turns of one agent never do.

## Speaking

Seats speak through the `opencompany` MCP server
(`src/hive/mcp_server.rs`), reached from a turn as `mcp_call_tool{server:
"opencompany", tool, arguments}`. The tool names, argument shapes and
descriptions are `tinyhivemind::speech::tool_specs()`, rendered verbatim:

| Tool | Utterance | What the host does |
| --- | --- | --- |
| `post {message}` | `Post` | one row for the whole desk |
| `broadcast {message}` | `Broadcast` | one row, then asks the router who should pick it up (`RunAgents`) |
| `dm {to: [ids], message}` | `Dm` | one row with `audience = to`; the recipients are assigned the next round (`DeliverDm`) |
| `complete_episode {message}` | `CompleteEpisode` | one row, and this seat's assignment is done |
| `read {limit}` | — | further back in this desk than the turn was handed |

`dm` recipients are checked at call time against `hive.resolve_dm`: a name
that is not an active seat on this desk is a tool **error**, and the seat
retries in the same turn. A DM is a narrowing *within* the room — the row is
on the desk, elided for a viewer not in its audience; an operator sees every
row. Reaching another desk is a [referral](#referral), never a `dm`.

The same server serves every OpenCompany tool an agent holds (ledger, tasks,
pages, workspace, memory, composio, hosting, approvals), because
`openhuman_embed::Agent` has no seam for an in-process host tool. The server
runs on a dedicated loopback listener; the route is `POST
/internal/mcp/{company}/{runtime_agent_id}` with a per-agent bearer minted at
build, and that bearer is the attribution: bearer → agent → the agent's one
in-flight turn → exactly one episode and round. Approvals are decided there
too — an OpenCompany tool call the agent's `ApprovalPolicy` parks journals
`ApprovalParked`, answers "awaiting approval", and its result is delivered as
a fresh turn once `ApprovalResolved` lands.

## The prompt

Every seat's turn message has a fixed first line — the sentinel the mock
brain and the tests key on — followed by the desk delta and the instruction:

```text
Hive turn: desk engineering, episode 7f3a…, round 2.

Since your last turn on this desk:
[41] operator: Ship the pricing page by Friday.
[42] engineer: !post I can have the backend flag ready Thursday.
[43] ceo (dm to you): Keep the copy short.

Your assignment: answer the operator's message; @writer has been invited.
Tools on server `opencompany`: post, broadcast, dm, complete_episode, read, record_entry, …
End this turn with exactly one `mcp_call_tool` on server `opencompany`:
post | broadcast | dm | complete_episode.
```

The delta is `tinyhivemind::sharing::prepare_delta` over
`EventLogSessionLog` — the journal read as a `SessionLog`, narrowed to this
desk and to what this seat may read — with attributed `SessionAuthor` lines.
The per-(agent, conversation) `SharingState` watermark is persisted with the
episode, so a 200-turn desk hands a seat only what it has not seen. A non-desk
turn gets the delta prefix and no fence. `revision` in the sentinel is raw
(0-based); the console shows it 1-based.

## Routing: Jev, then the lead

Who a message needs, and who a broadcast reaches, is asked of **Jev** —
TypeSafe's System One model — through `tinyhivemind_typesafe::JevRouter` over
the host transport `hive::jev::TinyHumansSystemOne`:

- `POST https://api.tinyhumans.ai/agent-integrations/openrouter/systemone`,
  bearer = the TinyHumans key resolved the way every managed call resolves it
  (`OPENCOMPANY_INFERENCE_KEY`, then the token file, then
  `TINYHUMANS_API_KEY`). The proxy speaks the System One wire body unchanged
  and resolves `jev-latest` to a concrete `typesafe/jev-*` id in the
  response; the router records that id and never compares it.
- `OPENCOMPANY_JEV_URL` moves the proxy. It must be `https`, or `http` to a
  loopback host — the credential is a request header, and the rule is the one
  [analytics.md](analytics.md) applies to its collector. A plain-`http` URL on
  a routable host is refused at boot.
- One immediate retry on a `429`/`529` (`classify_retry`) or any `5xx`; then
  the call fails and the round routes without Jev. A timeout (30 s) or a `4xx`
  fails at once.
- **No key, no router.** `hive::jev::jev_router` answers `None`, logged once,
  and every route is the fallback below.

The plan Jev returns is one of `One{primary}`, `Hive{primary, invited}`,
`Clarify{question}` or `Fallback{reason}`, journaled on `EpisodeOpened` and
`BroadcastRouted` as `plan` with `router: jev | fallback | explicit`. An
explicit `@mention` outranks Jev (`explicit`). Without Jev the fallback is
deterministic: an operator message goes to the desk lead (the first roster
member listed), and a broadcast goes to the next distinct participant in
scheduling order — which is why a company with no TinyHumans key still runs
every episode, only with less initiative in who picks a broadcast up.

`[group_chat.routing]` is the policy the router and the driver are frozen on
for the episode's life (`src/hive/routing.rs`):

```toml
[[group_chat]]
id = "engineering"
name = "Engineering desk"
members = ["engineer", "ceo"]

[group_chat.routing]
round_width = 2            # seats a round runs at once, including the primary
choice_option_limit = 8    # alternatives in one System One Choice, incl. `none`
max_rounds = 12            # rounds before the host closes the episode (`round_cap`)
turn_timeout_secs = 600    # one seat turn, counted from holding its turn_lock
minimum_confidence = 0.0   # Choice concentration below which Jev's pick is not taken
high_impact_minimum_confidence = 0.0
clarification_threshold = 1.0   # missing-information probability that asks instead of routing
high_impact_threshold = 1.0

[group_chat.routing.referral]
enabled = true             # off unless this says so
max_hops = 1               # chain depth; 2 is one round trip
reach = "desks"            # local | channels | desks — widens strictly
returns = true             # carry the answer back to the desk that asked
```

| Key | Default | Meaning |
| --- | --- | --- |
| `round_width` | `5` | seats the driver runs per round; Jev's `invited` list is clamped to it |
| `choice_option_limit` | `8` | alternatives per Choice; a larger roster is routed hierarchically |
| `max_rounds` | `12` | rounds before `EpisodeCompleted{reason: round_cap}` |
| `turn_timeout_secs` | `600` | one seat's turn; the lock wait is not counted |
| `minimum_confidence`, `high_impact_minimum_confidence` | `0.0` | the concentration a Choice needs before its pick is accepted |
| `clarification_threshold`, `high_impact_threshold` | `1.0` | when a plan becomes `Clarify` / when the high-impact rule applies |
| `referral.enabled` | `false` | may this desk put a question to another desk |
| `referral.max_hops` | `1` | chain depth |
| `referral.reach` | `desks` | `local` pulls a target into this room; `channels` runs them on their own desk; `desks` also lets `@#desk` convene that desk |
| `referral.returns` | `true` | the answer comes home |

A zero is refused at load rather than clamped (`routing.round_width = 0` is a
round of nobody; `max_rounds = 0` an episode that can never complete), and an
unknown `reach` word is named against the valid list. A manifest still
carrying the retired `[group_chat.hive]` block is refused with a migration
hint. The same block can be installed at runtime — `PUT
{scope}/desks/{id}/routing`, `DELETE` to restore the manifest, `GET` for the
declared block beside the effective numbers and the candidates — and every
write journals `DeskRoutingConfigured{desk_id, reset}` ([api.md](api.md#desk-routing-and-episodes)).
A running episode keeps the policy it opened with.

## Referral

Members of one desk read the same transcript and are wrong about the same
things, so the only pooling that helps is across the boundary. With
`[group_chat.routing.referral]` enabled, a `post` or `dm` that names a desk
(`@#content`) or a member of another desk is decided by
`tinyhivemind_core::referral::referral(policy, input, roster, desks)`; a
`MessageRoute::DeskReferral{desk_id}` is enqueued once — the journal's
`ReferralEnqueued` row keyed by `(episode, hop)` is the idempotency record —
and an episode opens on the target desk seeded with the question, authored
`hive-referral`.

When that episode completes, its answer is appended to the origin desk under
`HIVE_REFERRAL_AUTHOR` (`hive-referral`, hyphenated so no roster id can hold
it) and the origin episode resumes with the asker assigned. Only the answer
crosses: the far desk's own rounds stay on the far desk, under the seats that
took them. The `referral` frame carries both `episodeId` and `toEpisodeId`.

`reach = "local"` makes this exactly `mention_dispatch`: a target who is not
on this desk is pulled into this conversation for one turn. `max_hops = 1`
with `returns = true` is one question and one answer; a longer chain has to be
asked for.

## What lands in the journal

| Event | Fields | Projected |
| --- | --- | --- |
| `EpisodeOpened` | `chat_id, episode_id, opened_by_seq, parent_id?, participants, plan` | `episode_opened` |
| `RoundStarted` | `episode_id, revision, agent_ids` | `round_started` |
| `AgentReply` (+) | `episode: {id, revision, kind, to?, routed_by?}`, `audience` | `agent_reply` |
| `RoundCommitted` | `episode_id, revision, utterances[{agent_id, sequence, kind, message_seq?, to?}], actions` | `round_committed` |
| `BroadcastRouted` | `episode_id, revision, agent_id, message_seq, plan, probabilities?, router` | `broadcast_routed` |
| `DmDelivered` | `episode_id, from, to, message_seq` | `dm_delivered` |
| `EpisodeCompleted` | `episode_id, revision, completed_by?, rounds, reason, summary_seq?` | `episode_completed` |
| `EpisodeStateSaved` | `episode_id, desk, thread_root, revision, state, sharing` | never |
| `ReferralEnqueued` (+) | `episode_id?, to_episode_id?, hop` | `referral` |
| `DeskRoutingConfigured` | `desk_id, reset` (never the block) | `desk_routing_configured` |
| `TurnStarted` / `TurnFailed` (+) | `episode_id?, round_revision?, chat_id?, turn_id?` | `turn_started` / `turn_settled` (now on success too) |

`EpisodeOpened` and `EpisodeCompleted` are permanent; the round, broadcast,
dm and state rows are prunable, because the reply rows are the evidence and a
completed episode is rebuilt from them. `chat/history` alone rebuilds every
round after a reload; the frames add the present tense — which seats a round
opened with, which is still working. The frame shapes are in
[events.md](events.md#hive-episodes-and-rounds); the DTOs and
`GET {scope}/episodes` in [api.md](api.md#desk-routing-and-episodes).

Gone with the quorum hive: the `hive-report` and `hive-failure` authors, the
`!propose` / `!support` trace grammar, the closing summary row, private asides
(`asideConversation`), `DeskHiveConfigured` and `/desks/{id}/hive`.

## Failure

| Failure | Effect |
| --- | --- |
| a seat's turn errors or times out | `TurnFailed`, a synthetic `complete_episode("(no action)")` for that seat, the round commits |
| a seat makes no speech call | salvage as `post`, else up to four re-asks, then as above |
| a `dm` names a non-member | tool error to the seat; the turn continues |
| Jev unreachable | one retry, then the lead/mention fallback, logged; the round runs |
| a journal append fails | `EpisodeCompleted{reason: failed}`; the rows already appended are real |
| the roster changes under an open episode | `EpisodeCompleted{reason: membership_changed}`; the next message opens a fresh one |
| the host restarts mid-episode | resume from `EpisodeStateSaved` + replay; nothing runs twice |

## Where the code is

| Path | Holds |
| --- | --- |
| `src/hive/graph.rs` | `DeskHive` — `HiveGraph` + `AgentBinding`s per desk, rebuilt with the roster |
| `src/hive/driver.rs`, `round.rs` | `hive::dispatch`, the episode loop, seat turns, exactly-one enforcement |
| `src/hive/episode_store.rs` | `EpisodeStateSaved`, resume and replay |
| `src/hive/routing.rs` | `[group_chat.routing]` → `EffectiveRouting` → `RoutingPolicy`, the overlay, `RoutingPlanDto` |
| `src/hive/jev.rs` | `TinyHumansSystemOne`, `jev_router` |
| `src/hive/referral.rs` | the reserved `hive-referral` author, the pair key, the question and answer heads, `ReturnAddress` |
| `src/hive/session_log.rs` | `EventLogSessionLog` — the journal as a `SessionLog` |
| `src/hive/mcp_server.rs`, `tools.rs` | the `opencompany` MCP server, `InFlightRegistry`, the speech fold, `McpToolAdapter` |
| `src/harness/openhuman_runtime.rs` | the one `Runtime` |
| `src/harness/built_in/build.rs` | `agent_spec_for` |

## Testing

`tests/hive_e2e.rs` (gated `openhuman`, the `rust-gated` lane) boots a real
company — `RuntimeBuilder`, the embedded runtime, the filesystem store, the
HTTP surface — and drives it through `POST /api/v1/company/chat` against a
scripted OpenAI-compatible model that answers `tool_calls` for
`mcp_call_tool{server: "opencompany", tool, arguments}`:

| Test | What it proves |
| --- | --- |
| a two-member desk completes in two rounds | `EpisodeOpened`, two `RoundStarted`/`RoundCommitted` pairs, `EpisodeCompleted{complete_episode}` |
| a broadcast with no Jev falls back to the lead | `BroadcastRouted{router: fallback}` names the lead |
| a DM schedules its recipient | the row carries `audience`, the recipient is in the next round |
| a single-member desk | one ordinary reply, no round frames |
| a cross-desk referral | only the answer crosses, under `hive-referral` |
| **a shared agent on two desks** | both episodes complete; `turn_started` brackets overlap across desks and never for the same agent |
| crash after `RoundCommitted` | resume replays as a no-op |
| memory over MCP | `memory_store` in one episode, `memory_recall` in the next |

`src/hive/*_tests.rs` pin each seam without a model; `hive::jev` tests stand
up a fake proxy and assert the bearer, the URL rule, the one retry and the
timeout. The console's side is `npm run e2e:hive` against the mock brain
(`frontend/test/e2e/mock-brain.mjs`), and the numbers come from
`scripts/measure-coordination.sh` on `companies/hive_demo`.

## Measuring

Two readers fold the same frames into the same numbers, so neither has to be
trusted alone:

- `opencompany measure --company <id> [--data-dir <dir>] [--since <seq>]
  [--json] [--assert]` (`src/hive/measure.rs`) reads the journal through the
  env-selected storage backend, with no host running: turn brackets keyed by
  turn id give the peak of concurrent seat turns, the overlap count and the
  same-agent overlaps (must be zero); `EpisodeOpened` / `RoundStarted` /
  `EpisodeCompleted` give episodes opened and completed, rounds and time to
  complete per episode and the reason each closed; `BroadcastRouted`,
  `DmDelivered` and the forward `ReferralEnqueued` legs give broadcasts, dms,
  cross-desk referrals and the distinct agent pairs; `AgentReply.episode.kind`
  gives the utterance histogram. `--assert` exits with the number of missed
  thresholds.
- `scripts/measure-coordination.mjs` (thresholds and fold in
  `scripts/lib/coordination-metrics.mjs`) tails a live host's `/events` and
  cross-checks the peak against `GET /runs`, where every seat turn is a row
  carrying `episodeId` and `roundRevision`.

The thresholds are the plan's: max concurrent turns >= 2, >= 1 cross-desk
referral, >= 1 agent-to-agent dm or broadcast, >= 2 distinct pairs, no
same-agent overlap, every opened episode completed.

See also [speech.md](speech.md), [harnesses.md](harnesses.md),
[events.md](events.md#hive-episodes-and-rounds),
[api.md](api.md#desk-routing-and-episodes) and
[`docs/modules/hive/README.md`](../../modules/hive/README.md).
