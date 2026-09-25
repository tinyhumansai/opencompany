//! Hive desks: tinyhivemind's completion-driven episodes hosted over one
//! process-wide OpenHuman runtime (plan `hive-desks`).
//!
//! This file is an index. Each concern lives in its own module and is listed,
//! file by file, in this directory's `README.md`. The MCP server and the turn
//! registry under it are gated with the harness (`openhuman`): OpenCompany's
//! own tools are served to the agents over MCP because `openhuman_embed::Agent`
//! has no seam for an in-process host tool, and this crate's `openhuman-embed`
//! dependency enables `AgentSpec::mcp` unconditionally, so every harness build
//! — the `rust-gated` lane included — can attach the server. (The plan named
//! the `mcp` feature; that feature adds only OpenHuman's own MCP client
//! surface, which the server does not need, and gating on it would leave the
//! harness lane's turns without their tools.)

/// One completion episode on `tinyhivemind`'s own loop.
#[cfg(feature = "openhuman")]
pub mod conducted;
/// The chat body of the brain's cycle: which surface a message is on, and
/// the episode it opens on a desk with a room (Phase 5).
#[cfg(feature = "openhuman")]
pub mod dispatch;
/// An operator DM driven end to end: the hive, the surface, the dispatcher,
/// the seats. Needs the harness it drives, so it is gated with it.
#[cfg(all(test, feature = "openhuman"))]
#[path = "dm_episode_tests.rs"]
mod dm_episode_tests;
/// The journal as the episode store: the `GET {scope}/episodes` fold, the
/// driver checkpoint a resume reads, and the open-episode lookup (Phase 4).
pub mod episode_store;
/// One `OpenHumanHive` per desk over the company's live agents (Phase 4).
#[cfg(feature = "openhuman")]
pub mod graph;
/// This company as the host of one completion episode: the journal
/// `tinyhivemind` commits through, and how a teammate is built as a seat.
#[cfg(feature = "openhuman")]
pub mod host;
/// Jev routing over the TinyHumans System One proxy: the host-owned
/// `SystemOneTransport` and the `jev_router` constructor (plan Phase 7).
/// Gated with the harness whose credential seam it reads.
#[cfg(feature = "openhuman")]
pub mod jev;
/// The JSON-RPC Streamable-HTTP MCP server the company agents call their
/// speech and OpenCompany tools on (plan Phase 3).
#[cfg(feature = "openhuman")]
pub mod mcp_server;
/// Coordination metrics folded from the journal — concurrency, contacts,
/// completion — behind `opencompany measure` (Phase 8).
pub mod measure;
/// Cross-desk referral, read side: the reserved authors, the pair key, the
/// attribution heads, and the return address an answer comes home to.
pub mod referral;
/// The `[group_chat.routing]` block, its resolved `RoutingPolicy`, and the
/// desk-routing wire shapes (plan Phase 4).
pub mod routing;
#[cfg(feature = "openhuman")]
pub mod seating;
/// The company journal read as a tinyhivemind `SessionLog`, one desk at a
/// time (ex `hivemind/log.rs`).
pub mod session_log;
/// The in-flight turn registry, the speech fold and the tool adapter the
/// server dispatches through (plan Phase 3).
#[cfg(feature = "openhuman")]
pub mod shared_tool;
/// `take_over`: a guest seat claims work, concluding the conversation that
/// asked it and telling the operator in its own line.
#[cfg(feature = "openhuman")]
pub mod takeover;
#[cfg(test)]
pub(crate) mod test_support;
#[cfg(feature = "openhuman")]
pub mod tools;
