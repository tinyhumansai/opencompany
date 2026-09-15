//! What this host tells OpenHuman about tool packing.
//!
//! OpenHuman groups tools into **packs** and, by default, withholds a packed
//! tool's schema from any agent whose `agent_definition_name` is not one of that
//! pack's `owners` — advertising `use_skill` instead, so the agent has to go and
//! fetch the tool. That is a compression decision for a host whose per-turn cost
//! is dominated by tool schemas. It is the wrong one here.
//!
//! **OpenCompany does its own routing.** A tool's reach is decided by
//! [`policy::consequence`](crate::policy::consequence) and the approval gate,
//! not by whether OpenHuman put its schema on the wire. Withholding therefore
//! buys this host nothing and costs it the capability, which is exactly the
//! audience `ToolGroups::advertised` names: "a host that does not pay the
//! orchestrator's per-turn schema budget — a short-lived harness run, or an
//! embedder doing its own routing".
//!
//! # Why this exists at all
//!
//! The 2026-09 vendor bump filled six packs that were empty of anything this
//! host used — `files`, `media`, `profile`, `scheduling`, `storage`, `tasks` —
//! and `files` claims `file_read`, `file_write`, `grep`, `glob`, `list` and
//! `git_operations`. Those six were previously unpacked and advertised to
//! anyone with a broad grant. Their `owners` are OpenHuman's own archetypes
//! (`code_executor`, `planner`, …) and no OpenCompany teammate is spelled like
//! one, so every teammate lost raw file access outright.
//!
//! `strip_packed_from_visible` matches **by tool name alone**. It cannot tell
//! that this host constructs `FileReadTool` and `GrepTool` itself in
//! [`build`](super::build) — the names simply collide with the pack table. The
//! loss was a product regression, not a test artefact; it surfaced as 35 gated
//! failures on a lane green on `main`.
//!
//! # Why a call here rather than a `CoreContext`
//!
//! The posture used to be reachable only through
//! `CoreContext::init_with_config`, which is `async`, while every site that
//! reads it is synchronous: a roster build, an agent build (packs are stripped
//! in OpenHuman's `builder_build`, long before any turn), and this crate's own
//! `fn fixture()`. Standing a context up at the turn seam was tried and does not
//! work — it needs a raised `recursion_limit`, **4× the thread stack**, and
//! still leaves the belt stripped because the strip already happened at build
//! time. `set_process_default` (tinyhumansai/openhuman#6274) is the synchronous
//! seam that exists for this.
//!
//! # This does not widen any grant
//!
//! Pack withholding is a *presentation* filter applied after the belt is built.
//! Advertising changes which schemas reach the model; it does not add a tool to
//! any agent's belt, and every call still passes the grant check and the
//! approval gate. A tool compiled out, or off under the ambient `DomainSet`,
//! stays absent regardless — the three filters compose one way only, narrowing.

use openhuman_core as oh;

/// Declare this host's posture: every pack advertised.
///
/// Idempotent and cheap — `set_process_default` is `OnceLock`-backed upstream
/// and the first call wins, so every agent-build entry point calls it rather
/// than relying on one of them running first. Ordering between them is not
/// something this crate should have to reason about.
pub(crate) fn declare() {
    oh::tools::toolpacks::set_process_default(oh::tools::toolpacks::ToolGroups::advertised());
}
