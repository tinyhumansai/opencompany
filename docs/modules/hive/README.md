# Hive Module

`src/hive/` hosts tinyhivemind's completion-driven desks over the embedded
OpenHuman runtime: every `[[group_chat]]` is one `OpenHumanHive` with a
`CompletionDriver`, every member is an `openhuman_embed::Agent` bound into it,
and an operator message runs as an episode of concurrent rounds until a seat
reports it complete. The normative account — when a room opens, the round
loop, the speech tools, Jev routing, referral, what lands in the journal — is
[`docs/spec/runtime/hive.md`](../../spec/runtime/hive.md). This page is the
module's own shape and the reasoning behind its boundaries.

## Layout

| File | Holds |
| --- | --- |
| `mod.rs` | the index |
| `graph.rs` | `DeskHive{desk_id, hive: OpenHumanHive, roster_version}` — `HiveGraph` from `tinyhivemind_desks` / `tinyhivemind_roster`, one `AgentBinding` per effective member; held in `HarnessPool.hives` and rebuilt with the roster |
| `driver.rs` | `hive::dispatch(company_rt, stored_event)` — surface → `ConversationRef`, the non-desk single turn, and the episode loop |
| `round.rs` | one round: `tokio::spawn` per seat under its `turn_lock` and `turn_timeout_secs`, outbox drain, exactly-one enforcement, `apply_committed_round`, the `HostAction`s; `RoundBracket`, the `SeatBracket` the lock holder writes `TurnStarted` / `TurnSettled` and the seat's run row (`episodeId`, `roundRevision`) through |
| `measure.rs` | the coordination fold behind `opencompany measure` — concurrency peak and same-agent overlaps from the brackets, episodes / rounds / reasons, contacts and pairs, the utterance histogram; the thresholds mirror `scripts/lib/coordination-metrics.mjs` |
| `episode_store.rs` | `EpisodeStateSaved` — persisting `DriverState` + `SharingState`s, resume, replay of later `AgentReply` rows |
| `routing.rs` | `RoutingConfig` (`[group_chat.routing]`), `EffectiveRouting` → `RoutingPolicy` / `ReferralPolicy`, the overlay, `RoutingPlanDto`, `DeskRoutingDto` |
| `jev.rs` | `TinyHumansSystemOne: SystemOneTransport` over the TinyHumans System One proxy; `jev_router(env, key) -> Option<JevRouter<_>>` |
| `referral.rs` | the read side of a crossing: the reserved `hive-referral` author, the pair-conversation key, the question and answer heads, the `ReturnAddress` a far episode's checkpoint keeps |
| `session_log.rs` | `EventLogSessionLog` — the company journal read as a `tinyhivemind::SessionLog`, narrowed to one desk and one viewer |
| `mcp_server.rs` | the `opencompany` MCP server (`McpHost`): JSON-RPC over Streamable HTTP on a loopback listener, `POST /internal/mcp/{company}/{runtime_agent_id}`, one bearer per agent; `attach_opencompany_mcp` fixes it on an `AgentSpec` |
| `tools.rs` | `InFlightRegistry` (one in-flight turn per agent), the `InFlight` speech fold, `InFlightContext`, `McpToolAdapter` |
| `*_tests.rs` | one sibling per file; `jev_tests.rs` runs a fake proxy with `wiremock` |

Gated on `openhuman` as a whole (it drives the harness); `mcp_server.rs` and
`tools.rs` additionally on `mcp`, which is what brings `McpServer` to an
`AgentSpec`.

## The seams, and why they are where they are

**The library proposes; the host commits.** `CompletionDriver` never appends,
never waits, never runs a turn. `pending_round` is a proposal bound to one
`DriverState` revision; the host runs the seats, appends their rows, and only
then hands the committed utterances back with their real `EventSeq`s. The
sequence the library folds is the journal's own, so an episode rebuilt from
`chat/history` and an episode folded live cannot disagree.

**One agent, one lock, many hives.** A shared agent is one
`openhuman_embed::Agent` clone-bound into every hive that lists it. Its
`turn_lock` lives on the `CompanyAgent`, not on any hive, which is the whole
concurrency story: rounds of different desks are independent tasks, a seat
waits only on its own lock, and a turn never waits on another agent's lock —
so there is nothing to deadlock on. The timeout counts from lock acquisition
so a seat delayed by another desk is delayed, not failed.

**Speech is served, not wired.** `openhuman_embed::Agent` has no seam for an
in-process host tool, so `post` / `broadcast` / `dm` / `complete_episode` /
`read` and every OpenCompany tool reach the agent as
`mcp_call_tool{server: "opencompany", …}`. The per-agent bearer is the
attribution: it names the agent, the agent's one in-flight turn names the
episode and round, and a speech call folds into that turn's outbox. A second
speech call in one turn is an MCP error, which is how "exactly one action" is
enforced at the boundary rather than hoped for in the prompt.

**Routing is a policy frozen per episode.** `EffectiveRouting` is resolved
when the episode opens and travels with its `DriverState`; a `PUT
/desks/{id}/routing` mid-episode changes the next one. Jev is optional at the
type level — `Option<JevRouter<_>>` — and every path through `route_desk` and
`apply_committed_round` has the deterministic lead/next-participant fallback,
so a company with no TinyHumans key runs every episode.

**Referral composes, it does not extend.** One hive is one desk and `resume`
refuses another desk's episode (`OutOfHiveEpisode`). A cross-desk question is
therefore a second episode on the far hive, opened by `dispatch_referral`,
whose completion appends one row to the origin desk and resumes the origin
episode. The queue's idempotency record is the journal's own `ReferralEnqueued`
row, because a durable table nothing reads back would be state for its own
sake.

**Persistence is a ledger row, not a store.** `EpisodeStateSaved` is appended
after every commit and never projected. Resume is the latest row plus a
replay of newer `AgentReply` rows through `apply_committed`; the driver's
receipts make an exact replay a no-op, so the crash window between a commit
and its save costs nothing twice.

## Two contracts that are easy to break

**Append before fold.** The `EventSeq` an `AgentReply` append returns *is*
the `CommittedUtterance.sequence`. Folding a round with a guessed sequence and
appending afterwards would let a failed write leave the driver believing in a
row nothing can read back.

**A seat's failed turn is not the room's.** A seat that errors, times out or
makes no speech call gets `TurnFailed` and a synthetic
`complete_episode("(no action)")`; the round still commits and the episode
continues or completes. Only a failed *journal* append ends the episode.

## Running the tests

```bash
RUST_MIN_STACK=16777216 cargo test --features openhuman,mcp hive::
RUST_MIN_STACK=16777216 cargo test --features openhuman,mcp --test hive_e2e
```

The unit tests need no model: `jev` runs against a `wiremock` proxy, the
driver tests run on an ephemeral `Runtime` with the scripted HTTP model
(`tests/support/script_model.rs`), and `mcp_server` is driven with
`tinymcp::McpHttpClient`. `hive_e2e` boots a real company per test with a
unique company id (agent ids stay reserved on the runtime while any clone
lives).
