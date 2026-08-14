//! Company definition: the on-disk manifest and the entrypoints that load it.
//!
//! Phase 0 of the runtime: parse and validate `company.toml` / `agents.toml`,
//! surface problems in prosumer language, and boot a company far enough to
//! report its effective configuration. The cognition kernel (Brain, cycle
//! loop, stores) lands in later phases; see `docs/spec/roadmap.md`.

/// Issue #552: the seam between a task artifact and the shared workspace tree.
/// Always compiled — the console's workspace and artifact routes reach it in
/// every build, and only the publish drain's half is behind `openhuman`.
pub mod artifact_mirror;
pub mod company_key;
pub mod composio;
#[cfg(test)]
mod content_test;
// The workflow copilot's thread convention (issues #303, #416). Always
// compiled and openhuman-free: the chat handler reads it in every build to keep
// a copilot question from opening a board card, and the harness reads the same
// function to decide that a turn runs confined.
pub mod copilot;
// How this instance obtains its TinyHumans credential (projected, rotating
// platform token vs a static key). Always compiled: the answer decides whether a
// company can think at all, in every build.
pub mod credentials;
pub mod dns;
pub mod inference;
mod manifest;
pub mod mcp;
// Console MCP OAuth (issue #90): discovery + PKCE + DCR + token exchange for the
// per-tenant browser sign-in flow. Needs the vendored `oh::mcp::config_servers` discovery
// primitive + `uuid`/`base64`/`url`, so it links only under the `mcp` feature.
#[cfg(feature = "mcp")]
pub mod mcp_oauth;
pub(crate) mod owners;
pub mod runtime;
mod skill_file;
// Steer (issue #111): pause / cancel / redirect an in-flight task or delegation
// from the operator chat. Always compiled + openhuman-free so the operator
// control plane can steer in any build and no agent tool can ever reach it.
pub mod steer;
pub mod task_intent;
pub mod telegram;
mod types;
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
// Issue #607: text search over the shared tree, behind the agent
// `workspace_search` tool, the REST `GET …/workspace/search` route and the
// GraphQL `Company.workspaceSearch` resolver. Always compiled and openhuman-free
// for the same reason `workspace_links` is: two of its three callers are in the
// default build, and one shared scan is what stops them answering differently.
pub mod workspace_search;
// The workspace's `Agents/` + `Desks/` system roots, and the folders minted
// beneath them on first use (issue #551). Always compiled and openhuman-free:
// the scaffold is called from the runtime builder at boot, which is in the
// default build, and it touches nothing but the `WorkspaceStore` port.
pub mod workspace_scaffold;
pub mod workspace_seed;
// Issue #700: the operator-triggered removal of the empty `Agents/<id>/` folders
// a pre-#570 company still carries. Always compiled and openhuman-free, like the
// scaffold whose fail-closed root lookup it shares: its only caller is the
// console's REST route, and it touches nothing but the `WorkspaceStore` port.
pub mod workspace_sweep;

use std::path::Path;

pub use credentials::{Credential, CredentialSource, TinyhumansTokenSource, TokenTier};
/// The roster-id grammar check, shared with the runtime id minter so a slug and
/// a hand-authored `[[agent]].id` are held to one rule (issue #686). Not `pub`:
/// outside the crate the validator speaks through `CompanyManifest::validate`.
#[cfg(test)]
pub(crate) use manifest::is_snake_case;
pub use manifest::{DELEGATES_TO_WILDCARD, LEGACY_MANIFEST_FILE, Located, MANIFEST_FILE, discover};
pub use skill_file::{SkillDoc, load_dir_skills, parse_skill_md, render_skill_md};
pub use types::{
    Agent, BRAIN_MODES, Brain, Budget, ChannelConfig, Company, CompanyManifest, ComposioTools,
    Connection, DEFAULT_ALWAYS_APPROVE, DEFAULT_MAX_DELEGATION_DEPTH, DEFAULT_MAX_IN_FLIGHT_RUNS,
    DEFAULT_SEARCH_DAILY_CALLS, GATEABLE_NAMESPACES, INFERENCE_PROVIDERS, INFERENCE_TIERS,
    Inference, KNOWN_CHANNELS, MAX_DELEGATION_DEPTH_BOUNDS, McpServer, ORCHESTRATOR_TIER,
    PLAN_NAMES, PLAN_PERIODS, POLICY_MODES, PROVISIONED_POLICY_MODE, Place, Plan, Policy, Schedule,
    Skill, TIERS, TOOL_PROVIDERS, Tools, grants_composio_explicit, grants_media_explicit,
    grants_repo_explicit, grants_repo_write_explicit, grants_search_explicit,
    grants_workspace_write_explicit, orchestrator_id,
};
pub use workflow_file::{
    WORKFLOW_DESTINATION_KINDS, WORKFLOW_NODE_KINDS, WorkflowDestinationDef, WorkflowEdgeDef,
    WorkflowFile, WorkflowNodeDef, WorkflowNodeKind, WorkflowRetryDef, list_source_workflows,
    list_workflows_union, load_company_workflows, load_workflow_union, parse_workflow,
};
// Crate-internal only: the workflow creator (issue #69) builds a `RawWorkflow`
// from its request body, renders it to TOML, and re-parses it through
// `parse_workflow` above for validation before writing to disk.
pub(crate) use workflow_file::{
    RawEdge, RawNode, RawWorkflow, raw_workflow_from_toml, render_workflow,
    required_config_problems,
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
// Issue #580: the builder pass's courtesy validation, gated with the harness
// builder that is its only caller. Issue #753 adds `workflow_callable_tool_slugs`
// on the same footing — the create-time copilot's tool grounding.
#[cfg(feature = "openhuman")]
pub(crate) use workflow_create::{courtesy_validate_draft, workflow_callable_tool_slugs};
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

    match CompanyManifest::from_file(&located.path) {
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
