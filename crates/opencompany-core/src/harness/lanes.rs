//! Turning a company's declared `[[harness]]` set into the engines that serve
//! it.
//!
//! One place decides, for every declared harness, whether this host can run it
//! and what runs it — so the runtime builder does not grow a second opinion
//! about which agent lands where.
//!
//! ## One pool per `built_in` harness
//!
//! Each `built_in` harness gets its own [`HarnessPool`] and its own
//! [`HarnessDeps`], differing in exactly two fields: the provider (scoped to
//! that harness's config and credential slots) and
//! [`serves`](HarnessDeps::serves), which narrows the pool to the agents bound
//! to it.
//!
//! The narrowing is what makes one-pool-per-harness affordable. Without it every
//! pool would build every agent, so a ten-agent roster across three harnesses
//! would stand up thirty live agents — each holding a model client — to use ten.
//!
//! ## What is declared but not runnable
//!
//! An `acp` harness has no engine here yet: its transports live in the desktop
//! shell and the runner lane, and neither is wired into the server build. Rather
//! than silently routing those agents somewhere else, the harness is recorded as
//! unavailable with the reason, and a turn bound to it fails saying so. Falling
//! back would be the worst outcome available — the turn would succeed, on a
//! model and a credential nobody chose.
//!
//! This applies to the **default** harness exactly as much as a named one
//! (issue #1244). It used to not: every caller built the default lane straight
//! from `HarnessDeps`/`HarnessPool` on its own, without ever asking what kind
//! the default harness actually was, so a company whose *only* declared
//! harness was `kind = "acp"` still ran on the embedded engine — a silent
//! fallback of exactly the kind this module's own doctrine forbids. Resolving
//! the default the same way as every other harness, in this one place, is what
//! closes that gap for good instead of leaving a second opinion for a future
//! caller to reintroduce.
//!
//! ## `local` acp harnesses, when a factory is wired (issue #1245)
//!
//! `transport = "local"` now has a real engine wherever the caller supplies an
//! [`AcpAgentFactory`](crate::harness::acp::run_turn::AcpAgentFactory) — the
//! desktop shell, which owns the only implementation this crate does not
//! provide itself. A server build, or a desktop build asked to run a `runner`
//! harness (its socket transport is still unwired), passes `None`/leaves it
//! `unavailable` exactly as before.

use std::collections::HashSet;
use std::sync::Arc;

use crate::company::Harness;
use crate::company::inference::{EnvDefault, HarnessScope};
use crate::harness::built_in::provider::TenantProvider;
use crate::harness::built_in::run_turn::HarnessRunTurn;
use crate::harness::built_in::{HarnessDeps, HarnessPool};
use crate::ports::SecretStore;
use crate::ports::types::{CompanyId, CompanyRecord};
use crate::runtime::delegation::RunTurn;

/// The type `build`'s `acp_agents` parameter takes. Real under `acp`
/// (`crate::harness::acp::run_turn` — the `AcpAgent`/`AcpRunTurn` types — only
/// exists there); an uninhabited placeholder otherwise, so callers built
/// under plain `openhuman` (no `acp`) still compile and simply can never pass
/// `Some`.
#[cfg(feature = "acp")]
pub type AcpFactory<'a> = &'a dyn crate::harness::acp::run_turn::AcpAgentFactory;
#[cfg(not(feature = "acp"))]
pub type AcpFactory<'a> = &'a std::convert::Infallible;

/// Why a declared harness of `kind` has no engine on this host — the one
/// message both the default-harness path and the named-harness loop use, so
/// they cannot drift into saying different things about the same gap.
fn unavailable_reason(kind: &str) -> String {
    match kind {
        "acp" => "it is an ACP harness and this build has no ACP transport wired — \
                  run it from the desktop app, or bind these agents to a `built_in` harness"
            .to_string(),
        other => format!("`{other}` is not a harness kind this build knows how to run"),
    }
}

/// Resolves one `kind = "acp"` harness to an engine, or records why it has
/// none. Shared by the default-harness resolution and the named-harness loop
/// so the two cannot describe the same gap differently.
#[cfg(feature = "acp")]
fn resolve_acp_engine(
    harness: &Harness,
    acp_agents: Option<AcpFactory<'_>>,
    workspace_root: &std::path::Path,
    agent_models: &std::collections::HashMap<String, String>,
    desks: Vec<(String, String)>,
) -> std::result::Result<Arc<dyn RunTurn>, String> {
    // Validation guarantees `acp` is `Some` and `transport` is one of
    // `ACP_TRANSPORTS` on every harness that reaches here — this crate's own
    // `CompanyManifest::validate`, not a caller-supplied invariant.
    let acp = harness
        .acp
        .as_ref()
        .ok_or_else(|| unavailable_reason("acp"))?;

    if acp.transport != "local" {
        // `runner` (a remote socket dispatch) has no engine on any build yet —
        // a materially different, larger piece of work than the local
        // subprocess case, and out of scope here.
        return Err(
            "it uses `transport = \"runner\"` and this build has no runner transport wired yet"
                .to_string(),
        );
    }

    let factory = acp_agents.ok_or_else(|| unavailable_reason("acp"))?;
    let agent_id = acp.agent.as_deref().unwrap_or_default();
    factory
        .build(agent_id, acp.model.as_deref(), agent_models, workspace_root)
        .map(|agent| {
            Arc::new(crate::harness::acp::run_turn::AcpRunTurn::new(agent).with_desks(desks))
                as Arc<dyn RunTurn>
        })
        .map_err(|error| {
            // The reason reaches company chat and `warn` reaches Sentry
            // breadcrumbs, so neither carries the adapter's own error: it can
            // name a resolved binary path or its argv.
            tracing::warn!(acp_agent = %agent_id, "ACP adapter could not be started");
            tracing::debug!(acp_agent = %agent_id, %error, "ACP adapter start-up error");
            format!("its `{agent_id}` adapter could not be started on this host")
        })
}

/// The `openhuman`-without-`acp` build: unconditionally unavailable, exactly
/// as every `acp` harness was before issue #1245 — `acp_agents` can only ever
/// be `None` here (its type is uninhabited), so there is nothing to build.
#[cfg(not(feature = "acp"))]
fn resolve_acp_engine(
    _harness: &Harness,
    _acp_agents: Option<AcpFactory<'_>>,
    _workspace_root: &std::path::Path,
    _agent_models: &std::collections::HashMap<String, String>,
    _desks: Vec<(String, String)>,
) -> std::result::Result<Arc<dyn RunTurn>, String> {
    Err(unavailable_reason("acp"))
}

/// This company's `(desk id, desk name)` pairs, for an engine that has to
/// canonicalise a chat selector without a store — see
/// [`AcpRunTurn::session_key`](crate::harness::acp::run_turn).
fn declared_desks(record: &CompanyRecord) -> Vec<(String, String)> {
    // Manifest desks **and overlay desks**: a desk created from the console
    // lives only in `overlay_desks`, and `resolve_desk_id` / `GET .../desks`
    // both treat it as a routable desk, so leaving it out would mint two ACP
    // sessions for it under its two spellings — the exact split this snapshot
    // exists to prevent (codex + coderabbit on #1972).
    record
        .manifest
        .group_chats
        .iter()
        .map(|chat| (chat.id.clone(), chat.name.clone()))
        .chain(
            record
                .overlay_desks
                .iter()
                .map(|overlay| (overlay.id.clone(), overlay.name.clone())),
        )
        .collect()
}

/// The engines a company's declared harnesses resolve to on this host.
pub struct Lanes {
    /// Agents the **default** harness serves, when the company declares more
    /// than one. `None` means the whole roster — the single-harness case.
    pub default_serves: Option<HashSet<String>>,
    /// The engine for the default harness itself, when this host can run it.
    ///
    /// `None` if and only if the default harness's id has a matching entry in
    /// `unavailable` — callers must not substitute another engine in that
    /// case; see the module docs.
    pub default_engine: Option<Arc<dyn RunTurn>>,
    /// Every lane beyond the default: its harness id and the engine serving it.
    pub lanes: Vec<(String, Arc<dyn RunTurn>)>,
    /// Declared harnesses this host cannot run, and why. Includes the default
    /// harness's own id when `default_engine` is `None`.
    pub unavailable: Vec<(String, String)>,
}

/// Which agents are bound to `harness_id`, given the company's default.
fn agents_on(record: &CompanyRecord, harness_id: &str, default_harness: &str) -> HashSet<String> {
    // `effective_agents`, not `manifest.agents`: an admin's harness or model
    // edit to a blueprint teammate is stored as an overlay, so the raw
    // manifest row still says what the company launched with. Reading it here
    // meant a saved binding survived the write, survived a restart, and was
    // then ignored by the runtime that actually routes turns — the setting
    // persisted everywhere except where it mattered. It also drops retired
    // teammates, which have no business in a lane.
    let mut ids: HashSet<String> = record
        .effective_agents()
        .into_iter()
        .filter(|a| a.harness.as_deref().unwrap_or(default_harness) == harness_id)
        .map(|a| a.id)
        .collect();
    // A console-created (overlay) teammate carries its own optional binding
    // (issue #1245's harness-picker follow-up), resolved against the default
    // exactly like a manifest agent's. Folded in here rather than left for
    // some other pool to claim — every harness's serve set has to account for
    // its own overlay teammates, or a multi-harness company would build one
    // on no pool at all: the roster would silently drop a teammate the
    // console is still showing.
    ids.extend(
        record
            .overlay_agents
            .iter()
            .filter(|a| a.harness.as_deref().unwrap_or(default_harness) == harness_id)
            .map(|a| a.id.clone()),
    );
    ids
}

/// Per-agent model overrides for the agents [`agents_on`] returns for
/// `harness_id` — issue #1245's per-agent follow-up.
///
/// A `HashMap` rather than reusing `agents_on`'s `HashSet<String>`: an ACP
/// harness's factory needs the override *value*, not just which agents are
/// bound, and looking it back up by id from `record` at prompt time would
/// mean `LocalAcpAgent` holding a `&CompanyRecord` across turns rather than
/// the plain snapshot `resolve_acp_engine` already builds once. Only agents
/// with a model override appear here — a `built_in` agent's `model` is its
/// pair's model (keys rework slice 3a, issue #2306) and never reaches this
/// map, because the map is built only for `acp` harness ids and an agent
/// enters it only when bound to that id.
fn agent_models_on(
    record: &CompanyRecord,
    harness_id: &str,
    default_harness: &str,
) -> std::collections::HashMap<String, String> {
    // Effective, not raw — see `agents_on`. A model an admin picked in the
    // console lives in the overlay, and this map is what actually carries it
    // to the spawned harness.
    let mut models: std::collections::HashMap<String, String> = record
        .effective_agents()
        .into_iter()
        .filter(|a| a.harness.as_deref().unwrap_or(default_harness) == harness_id)
        .filter_map(|a| a.model.clone().map(|model| (a.id, model)))
        .collect();
    // Mirrors `agents_on`'s own overlay fold: an overlay teammate's own
    // binding decides which harness's map it enters, not an assumed default.
    models.extend(
        record
            .overlay_agents
            .iter()
            .filter(|a| a.harness.as_deref().unwrap_or(default_harness) == harness_id)
            .filter_map(|a| a.model.clone().map(|model| (a.id.clone(), model))),
    );
    models
}

/// Coding-CLI harness ids some agent binds to that no `[[harness]]` declares
/// — issue #1245's detected-harness follow-up.
///
/// Sorted and de-duplicated so a rebuild produces the same lane order rather
/// than whatever the roster happened to iterate in.
///
/// Reads **both** roster halves: an overlay teammate carries its own
/// `harness` binding now, and a console-added teammate on a detected CLI is
/// the whole point of the feature — missing it would leave that agent bound
/// to a lane nothing built.
fn referenced_implicit_locals(
    record: &CompanyRecord,
    declared: &[Harness],
    default_harness: &str,
) -> Vec<String> {
    // Effective, not raw — see `agents_on`. This decides which implicit-local
    // lanes get synthesized at all, so missing an overlay binding here leaves
    // the teammate bound to a lane nothing built.
    let effective = record.effective_agents();
    let manifest_bindings = effective.iter().filter_map(|a| a.harness.as_deref());
    let overlay_bindings = record
        .overlay_agents
        .iter()
        .filter_map(|a| a.harness.as_deref());

    let mut ids: Vec<String> = manifest_bindings
        .chain(overlay_bindings)
        .map(str::trim)
        // The default is resolved elsewhere and must not be shadowed here: a
        // company whose default *is* a declared `claude` harness would
        // otherwise get a second, synthesized lane of the same id.
        .filter(|id| *id != default_harness)
        .filter(|id| Harness::is_implicit_local_id(id))
        .filter(|id| !declared.iter().any(|h| h.id == *id))
        .map(str::to_string)
        .collect();
    ids.sort();
    ids.dedup();
    ids
}

/// Builds the lanes for `record`, given the shared pool and deps the
/// **default** harness runs on when it is runnable at all.
///
/// `default_serves` is `None` — "the whole roster, no narrowing" — for a
/// company that declares no `[[harness]]` (or declares exactly one): the
/// byte-identical single-pool path every existing company takes. That stays
/// true regardless of whether the default harness turns out to be runnable;
/// what changed (issue #1244) is that `default_engine`/`unavailable` are now
/// always resolved too, instead of every caller resolving the default
/// separately (and inconsistently) on its own.
pub fn build(
    record: &CompanyRecord,
    pool: Arc<HarnessPool>,
    base: &HarnessDeps,
    secrets: Arc<dyn SecretStore>,
    env_default: Option<EnvDefault>,
    acp_agents: Option<AcpFactory<'_>>,
) -> Lanes {
    let declared = record.manifest.effective_harnesses();
    let default_harness = record.manifest.default_harness();
    let default_harness_id = default_harness.id.clone();

    let mut lanes = Vec::new();
    let mut unavailable = Vec::new();

    let default_engine = match default_harness.kind.as_str() {
        // The base deps already resolved the default's own `[harness.inference]`
        // precedence (`default_harness_inference`) before this was called —
        // wrap them in the caller's shared pool exactly as it always did.
        "built_in" => {
            Some(Arc::new(HarnessRunTurn::new(pool, Arc::new(base.clone()))) as Arc<dyn RunTurn>)
        }
        "acp" => match resolve_acp_engine(
            &default_harness,
            acp_agents,
            &base.workspace_root,
            &agent_models_on(record, &default_harness_id, &default_harness_id),
            declared_desks(record),
        ) {
            Ok(engine) => Some(engine),
            Err(reason) => {
                unavailable.push((default_harness_id.clone(), reason));
                None
            }
        },
        kind => {
            unavailable.push((default_harness_id.clone(), unavailable_reason(kind)));
            None
        }
    };

    for harness in declared.iter().filter(|h| h.id != default_harness_id) {
        match harness.kind.as_str() {
            "built_in" => lanes.push((
                harness.id.clone(),
                built_in_lane(
                    record,
                    base,
                    &secrets,
                    env_default.clone(),
                    harness,
                    &default_harness_id,
                ),
            )),
            "acp" => match resolve_acp_engine(
                harness,
                acp_agents,
                &base.workspace_root,
                &agent_models_on(record, &harness.id, &default_harness_id),
                declared_desks(record),
            ) {
                Ok(engine) => lanes.push((harness.id.clone(), engine)),
                Err(reason) => unavailable.push((harness.id.clone(), reason)),
            },
            kind => unavailable.push((harness.id.clone(), unavailable_reason(kind))),
        }
    }

    // Coding CLIs bound by name without any `[[harness]]` declaring them
    // (issue #1245's detected-harness follow-up). Built **on demand** — only
    // for an id some agent actually references — and that is load-bearing
    // rather than an optimization: `HarnessBrain::run_turn` returns the plain
    // engine when `lanes` *and* `unavailable` are both empty, so synthesizing
    // a lane per known CLI for every company would take every company off
    // that path. A company that binds nobody to one adds nothing here.
    for id in referenced_implicit_locals(record, &declared, &default_harness_id) {
        let harness = Harness::implicit_local(&id);
        match resolve_acp_engine(
            &harness,
            acp_agents,
            &base.workspace_root,
            &agent_models_on(record, &id, &default_harness_id),
            declared_desks(record),
        ) {
            Ok(engine) => lanes.push((id, engine)),
            Err(reason) => unavailable.push((id, reason)),
        }
    }

    // `lanes.is_empty()` joins the old `declared.len() <= 1` test rather than
    // replacing it: a company that declares one harness but binds somebody to
    // a detected CLI now has somewhere else for an agent to land, so the
    // default pool must be narrowed to the agents that actually stay on it.
    // Every previously-existing case resolves identically.
    let default_serves = if declared.len() <= 1 && lanes.is_empty() {
        None
    } else {
        Some(agents_on(record, &default_harness_id, &default_harness_id))
    };

    Lanes {
        default_serves,
        default_engine,
        lanes,
        unavailable,
    }
}

/// One `built_in` lane: its own pool, over deps carrying its own provider and
/// narrowed to the agents bound to it.
fn built_in_lane(
    record: &CompanyRecord,
    base: &HarnessDeps,
    secrets: &Arc<dyn SecretStore>,
    env_default: Option<EnvDefault>,
    harness: &Harness,
    default_harness: &str,
) -> Arc<dyn RunTurn> {
    // Its own `[harness.inference]`, else the company-level `[inference]` — the
    // caller cannot pick, because only the harness knows whether it declared
    // one.
    let manifest_inference = harness
        .inference
        .clone()
        .unwrap_or_else(|| record.manifest.inference.clone());

    let provider = Arc::new(
        TenantProvider::new(
            record.id.clone(),
            secrets.clone(),
            manifest_inference,
            env_default,
        )
        .with_scope(
            HarnessScope::named(&harness.id)
                // Reported here because this is the only place that still knows.
                // Two lines up the two sources are merged into one value, and
                // the resolver has to tell them apart to decide whether the
                // company's provider list outranks this harness.
                .declaring_own_inference(harness.inference.is_some()),
        ),
    );

    let mut deps = base.clone();
    deps.provider = provider;
    deps.serves = Some(agents_on(record, &harness.id, default_harness));

    Arc::new(HarnessRunTurn::new(
        Arc::new(HarnessPool::new()),
        Arc::new(deps),
    ))
}

/// The company id a lane set was built for. Exposed so a caller can assert it
/// wired the lanes it thinks it did.
pub fn company_of(record: &CompanyRecord) -> &CompanyId {
    &record.id
}

#[cfg(test)]
#[path = "lanes_tests.rs"]
mod tests;
