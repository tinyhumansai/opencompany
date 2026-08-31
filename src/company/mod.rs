//! Company definition: the on-disk manifest and the entrypoints that load it.
//!
//! Phase 0 of the runtime: parse and validate `company.toml` / `agents.toml`,
//! surface problems in prosumer language, and boot a company far enough to
//! report its effective configuration. The cognition kernel (Brain, cycle
//! loop, stores) lands in later phases; see `docs/spec/roadmap.md`.

// Per-teammate roster files (`companies/<name>/agents/<id>.toml`) — the richer
// alternative to inline `[[agent]]` entries, carrying a custom prompt and its
// own briefing documents. Always compiled: it is part of parsing a company, and
// `opencompany check` must report on it in every build.
/// The account-activation funnel (issue #1843): whether a company has confirmed
/// its name, connected + granted Composio, and run a real workflow to success —
/// the shared substrate the onboarding gate and the week-1 nudge both read.
/// Always compiled: the REST read projection is a default-build console route,
/// and gating the derivation behind a feature its caller lacks is exactly how
/// `create_company_workflow` (issue #168) and this module's own name-confirmed
/// input drifted apart before.
pub(crate) mod activation;
pub(crate) mod agent_file;
/// Issue #552: the seam between a task artifact and the shared workspace tree.
/// Always compiled — the console's workspace and artifact routes reach it in
/// every build, and only the publish drain's half is behind `openhuman`.
pub mod artifact_mirror;
/// Avatar references: which face a teammate or a person wears when somebody has
/// chosen one (`docs/spec/runtime/avatars.md`). Always compiled — the team and
/// user write planes validate through it in every build, and the rule it
/// enforces (an avatar names something this host holds, never a URL) is a
/// control rather than a convenience.
pub mod avatar;
pub mod company_key;
pub mod composio;
#[cfg(test)]
mod content_test;
// Which workspace documents each role is told to reason from
// (`docs/spec/runtime/orchestration/context-routing.md`). Always compiled: the
// per-tier default table and the class-based exclusions are pure decisions over
// manifest data, and the exclusions are controls — they deserve tests in every
// build, not only where the agent runtime links.
pub mod context_routing;
// The workflow copilot's thread convention (issues #303, #416). Always
// compiled and openhuman-free: the chat handler reads it in every build to keep
// a copilot question from opening a board card, and the harness reads the same
// function to decide that a turn runs confined.
pub mod copilot;
// How this instance obtains its TinyHumans credential (projected, rotating
// platform token vs a static key). Always compiled: the answer decides whether a
// company can think at all, in every build.
pub mod billing;
pub mod credentials;
pub mod dns;
pub mod hosting;
pub mod inference;
/// Ledger declaration files: `companies/<name>/ledgers/<slug>.toml`. A vertical
/// ships the axes it is about — a matter list, a deal pipeline, an experiment
/// log — the way it already ships its roster, rather than waiting for some turn
/// to think of declaring one.
pub mod ledger_file;
/// Dynamic ledgers: the one place a ledger is read, written, declared or
/// retired. Every surface routes through it, because the rules that matter —
/// only a person deletes, a close says why, the derived file follows the
/// write — only hold if exactly one code path enforces them.
pub mod ledgers;
mod manifest;
pub mod mcp;
/// The bundle's MCP declaration file: `companies/<name>/mcp.json`. A vertical
/// ships the tool servers its work needs the way it already ships its ledgers,
/// rather than starting with an empty tool surface somebody has to fill in by
/// hand from the console before the company can do anything.
pub mod mcp_file;
pub mod paypal;
// Console MCP OAuth (issue #90): discovery + PKCE + DCR + token exchange for the
// per-tenant browser sign-in flow. Needs the vendored `oh::mcp::config_servers` discovery
// primitive + `uuid`/`base64`/`url`, so it links only under the `mcp` feature.
#[cfg(feature = "mcp")]
pub mod mcp_oauth;
// How an agent's system prompt is composed from its manifest definition and the
// documents routed to it. Always compiled and runtime-free: the harness that
// spends the prompt is behind `openhuman`, but the composition and budget-clamp
// rules are ordinary text handling with real edge cases, and they are worth
// testing in the default build rather than only where the agent runtime links.
pub mod prompt;
// The shape of one drafted teammate mandate or persona (issue #1776). Same
// always-compiled argument as `prompt` above: the model call that produces a
// draft is behind `openhuman`, but what a draft IS — which fields are
// draftable, the bound each obeys, and the three distinct reasons there might
// be no draft — is ordinary data handling the default build should test.
pub mod profile_draft;
// Rendering that composition back out for a human, from a manifest alone. Same
// always-compiled argument as `prompt` above, one step further: a debugging
// surface that only existed in a `--features openhuman` build is one nobody
// runs, so the default build renders what it can and names the rest.
pub mod prompt_dump;
pub mod runtime;
// Per-company web search configuration: which provider a company's agents
// search through, and the BYO credential behind it. Keys only — the tools that
// spend them live behind `openhuman` in `crate::harness::search_byo`.
pub mod search;
// First-run company setup (issue: docs/spec/runtime/company-setup.md): the
// curated starting rosters and the rules a proposed roster obeys. Always
// compiled and model-free on purpose — it is both the input to the optional
// polish pass and the fallback when that pass cannot run, so a company with no
// inference credential still gets a real team.
pub mod setup;
mod skill_file;
// Steer (issue #111): pause / cancel / redirect an in-flight task or delegation
// from the operator chat. Always compiled + openhuman-free so the operator
// control plane can steer in any build and no agent tool can ever reach it.
pub mod steer;
/// Seed board cards: `globals/tasks.toml` and `companies/<name>/tasks.toml`. A
/// company boots with the setup work it obviously has already on the board,
/// rather than with an empty To-do column and agents that have nothing to pick
/// up.
pub mod task_file;
pub mod task_intent;
// The one list of tools a company can grant — built-ins, MCP servers and
// Composio toolkits in a single vocabulary. Always compiled: it is a projection
// over the manifest, and the console route that renders it is in the default
// build.
pub mod tool_catalog;
mod types;
/// The week-1 "did this user save a workflow" query (issue #1845): the
/// per-user attribution check
/// [`runtime::LifecycleScheduler`](crate::runtime::LifecycleScheduler) reads
/// before nudging. Always compiled — it is a pure journal scan, no different
/// from `activation`'s own workflow-run check.
pub(crate) mod week1_nudge;
mod workflow_create;
mod workflow_file;
// The shared workspace-file read (node + content + `[[wikilink]]` backlinks)
// behind both the GraphQL `workspaceFile` resolver and the REST
// `GET …/workspace/file/{id}` route the console calls. Always compiled: the
// REST route is in the default build, and one shared scan is what keeps the two
// read surfaces from drifting.
pub(crate) mod workspace_links;
// How a node's logical path is rendered from its ancestor chain, and how a
// caller-supplied one is validated. Shared by the agent tools' `PathIndex` and
// by `workspace_search`, so a node search offers is always a node
// `workspace_read` can open. Always compiled: search reaches the default-build
// REST and GraphQL surfaces, the tools do not.
pub(crate) mod workspace_paths;
// Issue #759: the operator-triggered merge of the duplicate sibling folders a
// publish race already left behind, and the report of what it refused to
// decide. Always compiled and openhuman-free, for the same reason the #700 sweep
// beside it is: its only caller is the console's REST route, and it touches
// nothing but the `WorkspaceStore` port.
pub mod workspace_repair;
// The one naming rule for everything the runtime puts in a workspace: lowercase,
// dashed. Always compiled and dependency-free — the scaffold, the publish
// mirror, the page tools and the workspace write tools all mint names, and one
// shared rule is what stops them minting four different spellings.
pub mod workspace_names;
// Issue #607: text search over the shared tree, behind the agent
// `workspace_search` tool, the REST `GET …/workspace/search` route and the
// GraphQL `Company.workspaceSearch` resolver. Always compiled and openhuman-free
// for the same reason `workspace_links` is: two of its three callers are in the
// default build, and one shared scan is what stops them answering differently.
pub mod workspace_search;
// The workspace's `agents/` + `desks/` system roots, and the folders minted
// beneath them on first use (issue #551). Always compiled and openhuman-free:
// the scaffold is called from the runtime builder at boot, which is in the
// default build, and it touches nothing but the `WorkspaceStore` port.
pub mod workspace_scaffold;
pub mod workspace_seed;
// Issue #700: the operator-triggered removal of the empty `agents/<id>/` folders
// a pre-#570 company still carries. Always compiled and openhuman-free, like the
// scaffold whose fail-closed root lookup it shares: its only caller is the
// console's REST route, and it touches nothing but the `WorkspaceStore` port.
pub mod workspace_sweep;

use std::path::Path;

pub use credentials::{Credential, CredentialSource, TinyhumansTokenSource, TokenTier};
pub use ledger_file::{LEDGERS_DIR, has_ledger_files, load_dir_ledgers};
/// The roster-id grammar check, shared with the runtime id minter so a slug and
/// a hand-authored `[[agent]].id` are held to one rule (issue #686). Not `pub`:
/// outside the crate the validator speaks through `CompanyManifest::validate`.
#[cfg(test)]
pub(crate) use manifest::is_snake_case;
pub use manifest::{DELEGATES_TO_WILDCARD, LEGACY_MANIFEST_FILE, Located, MANIFEST_FILE, discover};
pub use mcp_file::{MCP_FILE, has_mcp_file, load_dir_mcp_servers};
pub use skill_file::{SkillDoc, load_dir_skills, parse_skill_md, render_skill_md};
pub use task_file::{TASKS_FILE, TaskSeed, has_task_file, load_dir_tasks};
pub use types::{
    ACP_AGENTS, ACP_TRANSPORTS, AcpHarness, Agent, BRAIN_MODES, Brain, Budget, ChannelConfig,
    Company, CompanyManifest, ComposioTools, Connection, ContextAccess, ContextEntry,
    CreationGrant, DEFAULT_ALWAYS_APPROVE, DEFAULT_HARNESS_KIND, DEFAULT_MAX_DELEGATION_DEPTH,
    DEFAULT_MAX_IN_FLIGHT_RUNS, DEFAULT_SEARCH_DAILY_CALLS, GATEABLE_NAMESPACES, GroupChat,
    HARNESS_KINDS, Harness, IMPLICIT_HARNESS_ID, INFERENCE_PROVIDERS, INFERENCE_TIERS, Inference,
    KNOWN_CHANNELS, LedgerAccess, LedgerGrant, MAX_DELEGATION_DEPTH_BOUNDS, McpServer,
    ORCHESTRATOR_TIER, PLAN_NAMES, PLAN_PERIODS, POLICY_MODES, PROMPT_CLASSES,
    PROMPT_FILE_BUDGET_CHARS, PROVISIONED_POLICY_MODE, Place, Plan, Policy, Schedule, Skill, TIERS,
    TOOL_PROVIDERS, Tools, creation_default_grants, grants_chargebee_explicit,
    grants_composio_explicit, grants_confer_native, grants_files_or_docs, grants_hosting_explicit,
    grants_media_explicit, grants_paypal_explicit, grants_search_explicit,
    grants_workspace_write_explicit, native_capability_namespaces, orchestrator_id,
};
pub use workflow_file::{
    STAGELESS_SCHEDULE_REFUSAL, STAGELESS_WORKFLOW_NOTICE, UNDELIVERABLE_SCHEDULE_REFUSAL,
    WORKFLOW_DESTINATION_KINDS, WORKFLOW_NODE_KINDS, WorkflowDestinationDef, WorkflowEdgeDef,
    WorkflowFile, WorkflowNodeDef, WorkflowNodeKind, WorkflowPostconditionDef, WorkflowRetryDef,
    destination_is_reachable, list_source_workflows, list_workflows_union,
    list_workflows_with_globals, load_company_workflows, load_workflow_union,
    load_workflow_with_globals, parse_workflow,
};
// Crate-internal only: the workflow creator (issue #69) builds a `RawWorkflow`
// from its request body, renders it to TOML, and re-parses it through
// `parse_workflow` above for validation before writing to disk.
pub(crate) use workflow_file::{
    RawEdge, RawNode, RawWorkflow, channel_destination_missing_target_message,
    raw_workflow_from_toml, render_workflow, required_config_problems,
};
// Issue #661 (M7): the read half of the agent workflow-admin surface — a stored
// graph projected onto the narrow agent authoring schema, plus the policy
// residue that schema cannot carry. Lives with the parser it reads, and stays
// crate-internal like every other raw-shape helper above.
#[cfg(feature = "openhuman")]
pub(crate) use workflow_file::{WorkflowSpecProjection, project_workflow_spec};
// Crate-internal only: the shared validated-persist core (issue #112) both the
// REST `POST …/workflows` route and the orchestrator `create_workflow` tool run.
// Ungated: the REST route is in the default build, so gating this behind
// `openhuman` is what let the two surfaces drift apart (issue #168).
pub(crate) use workflow_create::{
    WorkflowGraphSpec, create_company_workflow, delete_company_workflow, raw_workflow_from_spec,
    rollback_company_workflow, seed_file_exists, set_company_workflow_enabled,
    update_company_workflow, workflow_version,
};
// Issue #580: the builder pass's courtesy validation. Ungated since issue #1074
// for the same reason `create_company_workflow` above is: its second caller is
// the REST `POST …/workflows/validate` route, which is in the default build, and
// a shared validator gated behind a feature its caller lacks is how the two
// create surfaces drifted apart in #168.
pub(crate) use workflow_create::courtesy_validate_draft;
// Issue #753: the copilot's tool grounding, gated with the harness builder that
// is its only caller. Split by #874 into the effective set a proposal may name
// and the granted-but-unwired remainder that is reported, not offered.
#[cfg(feature = "openhuman")]
pub(crate) use workflow_create::{
    workflow_effective_tool_slugs, workflow_granted_but_unwired_tool_slugs,
    workflow_graph_from_spec, workflow_spec_from_graph,
};
pub use workspace_seed::{NodeKind, SeedNode, extract_wikilinks, walk_workspace};

use crate::{Result, VERSION};

/// Loads a company from a manifest path (a file or a directory containing one)
/// and boots it far enough to report its effective configuration.
///
/// In Phase 0 this validates the manifest and prints a boot banner; the
/// cognition kernel is wired in later phases. Example harnesses call this in
/// place of printing raw TOML.
pub fn run_company(path: impl AsRef<Path>) -> Result<()> {
    let manifest = CompanyManifest::from_path(path)?;
    println!(
        "OpenCompany v{VERSION} — booting `{}`\n",
        manifest.company.name
    );
    print!("{}", manifest.effective_summary());
    Ok(())
}

/// Validates a manifest for `opencompany check`, printing a deprecation note
/// for legacy filenames, the effective config on success, or every problem on
/// failure. Returns `true` when the manifest is valid.
pub fn run_check(path: &Path) -> bool {
    let located = match discover(path) {
        Ok(located) => located,
        Err(err) => {
            eprintln!("{err}");
            return false;
        }
    };

    if located.legacy {
        println!(
            "⚠ {} uses the legacy `agents.toml` name — rename it to `company.toml` when convenient.\n",
            located.path.display()
        );
    }

    // `from_located`, not `from_file`: a bundle whose roster lives in
    // `agents/*.toml` must be validated with that roster loaded, or every desk
    // member reads as "not an agent in the roster".
    match CompanyManifest::from_located(&located) {
        Ok(manifest) => {
            println!("✓ {} — valid\n", located.path.display());
            print!("{}", manifest.effective_summary());
            true
        }
        Err(err) => {
            eprintln!("{err}");
            false
        }
    }
}
