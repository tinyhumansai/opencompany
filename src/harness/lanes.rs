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
            Arc::new(crate::harness::acp::run_turn::AcpRunTurn::new(agent)) as Arc<dyn RunTurn>
        })
        .map_err(|error| format!("`{agent_id}` could not be started: {error}"))
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
) -> std::result::Result<Arc<dyn RunTurn>, String> {
    Err(unavailable_reason("acp"))
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
/// with a model override appear here — `CompanyManifest::validate` already
/// confirmed every override sits on an `acp`-bound agent, so a `built_in`
/// company's agents never enter this map at all.
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
        .with_scope(HarnessScope::named(&harness.id)),
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
mod tests {
    use super::*;
    use crate::ports::types::OverlayAgent;

    /// A two-harness company with a console-created overlay teammate.
    fn record() -> CompanyRecord {
        let manifest: crate::company::CompanyManifest = toml::from_str(
            r#"
[company]
name = "Acme"

[[agent]]
id = "ceo"
role = "Chief Executive"

[[agent]]
id = "researcher"
role = "Researcher"
harness = "deep"

[[harness]]
id = "embedded"
kind = "built_in"
default = true

[[harness]]
id = "deep"
kind = "built_in"
"#,
        )
        .expect("valid manifest");
        CompanyRecord {
            overlay_retired_agents: Vec::new(),
            overlay_agent_edits: Vec::new(),
            id: CompanyId::new("acme"),
            manifest,
            ledger: Vec::new(),
            lifecycle: "running".to_string(),
            overlay_agents: vec![OverlayAgent {
                id: "writer".into(),
                name: "Writer".into(),
                role: "Content Writer".into(),
                description: None,
                tools: None,
                model: None,
                harness: None,
            }],
            overlay_desk_members: Vec::new(),
            overlay_desk_order: Vec::new(),
            overlay_desks: Vec::new(),
            overlay_workflows: Vec::new(),
            overlay_budgets: Vec::new(),
            overlay_policy: None,
            overlay_tool_grants: None,
            overlay_desk_tools: Default::default(),
            disabled_workflows: Vec::new(),
            template_provenance: None,
            setup: None,
            name_confirmed: false,
            activation_completed_at: None,
            created_at_millis: None,
        }
    }

    /// The default lane serves the whole default-bound roster **including**
    /// every overlay teammate, whose only harness is the default.
    #[test]
    fn the_default_lane_serves_every_overlay_agent() {
        let rec = record();
        let default = agents_on(&rec, "embedded", "embedded");
        assert!(default.contains("ceo"));
        assert!(!default.contains("researcher"), "bound to the deep lane");
        assert!(
            default.contains("writer"),
            "a console-created teammate runs on the default harness"
        );

        // And the named lane must not claim it — the overlay is nobody's but
        // the default's.
        let deep = agents_on(&rec, "deep", "embedded");
        assert!(deep.contains("researcher"));
        assert!(!deep.contains("writer"));
        assert!(!deep.contains("ceo"));
    }

    /// **The property the whole detected-harness design rests on** (issue
    /// #1245's follow-up): a company that binds nobody to a coding CLI
    /// synthesizes nothing.
    ///
    /// Not a tidiness assertion. `HarnessBrain::run_turn` returns the plain
    /// engine when `lanes` *and* `unavailable` are both empty, so folding a
    /// lane per known CLI into every company would quietly take every
    /// company in every deployment off that path — and on a server build
    /// (no ACP factory) leave three `unavailable` entries behind as well.
    #[test]
    fn an_unreferenced_coding_cli_synthesizes_no_lane() {
        let rec = record();
        let declared = rec.manifest.effective_harnesses();
        assert!(
            referenced_implicit_locals(&rec, &declared, "embedded").is_empty(),
            "nobody names a coding CLI, so nothing is synthesized"
        );
    }

    /// A binding to a coding CLI no `[[harness]]` declares is picked up from
    /// **either** roster half — a console-added teammate on a detected CLI is
    /// the case the feature exists for.
    #[test]
    fn a_referenced_coding_cli_is_synthesized_from_either_roster_half() {
        let mut rec = record();
        rec.manifest.agents[0].harness = Some("claude".into());
        rec.overlay_agents[0].harness = Some("codex".into());
        let declared = rec.manifest.effective_harnesses();

        // Sorted and de-duplicated, so a rebuild keeps the same lane order.
        assert_eq!(
            referenced_implicit_locals(&rec, &declared, "embedded"),
            vec!["claude".to_string(), "codex".to_string()]
        );

        // And each is a `local` acp harness the ACP path can resolve.
        let synthesized = Harness::implicit_local("claude");
        assert_eq!(synthesized.kind, "acp");
        assert!(!synthesized.default, "never the company default");
        assert_eq!(synthesized.acp.expect("acp").transport, "local".to_string());
    }

    /// A declared harness of the same id is never shadowed by a synthesized
    /// one — otherwise a company that deliberately pinned a model on its own
    /// `claude` harness would get a second, bare lane of the same name.
    #[test]
    fn a_declared_or_default_coding_cli_is_not_synthesized_twice() {
        let mut rec = record();
        rec.manifest.agents[0].harness = Some("claude".into());

        // Declared under that exact id.
        let declared = vec![
            Harness::implicit(),
            Harness {
                id: "claude".into(),
                kind: "acp".into(),
                default: false,
                inference: None,
                acp: None,
            },
        ];
        assert!(
            referenced_implicit_locals(&rec, &declared, "embedded").is_empty(),
            "the declared harness wins"
        );

        // And when it *is* the default, which is resolved separately.
        assert!(
            referenced_implicit_locals(&rec, &[], "claude").is_empty(),
            "the default is resolved by the default path, not synthesized here"
        );
    }
}
