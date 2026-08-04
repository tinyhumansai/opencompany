//! Company definition: the on-disk manifest and the entrypoints that load it.
//!
//! Phase 0 of the runtime: parse and validate `company.toml` / `agents.toml`,
//! surface problems in prosumer language, and boot a company far enough to
//! report its effective configuration. The cognition kernel (Brain, cycle
//! loop, stores) lands in later phases; see `docs/spec/roadmap.md`.

pub mod composio;
#[cfg(test)]
mod content_test;
// How this instance obtains its TinyHumans credential (projected, rotating
// platform token vs a static key). Always compiled: the answer decides whether a
// company can think at all, in every build.
pub mod credentials;
pub mod dns;
pub mod inference;
mod manifest;
pub mod mcp;
// Console MCP OAuth (issue #90): discovery + PKCE + DCR + token exchange for the
// per-tenant browser sign-in flow. Needs the vendored `oh::mcp_client` discovery
// primitive + `uuid`/`base64`/`url`, so it links only under the `mcp` feature.
#[cfg(feature = "mcp")]
pub mod mcp_oauth;
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
pub mod workspace_seed;

use std::path::Path;

pub use credentials::{Credential, CredentialSource, TinyhumansTokenSource, TokenTier};
pub use manifest::{LEGACY_MANIFEST_FILE, Located, MANIFEST_FILE, discover};
pub use skill_file::{SkillDoc, load_dir_skills, parse_skill_md, render_skill_md};
pub use types::{
    Agent, BRAIN_MODES, Brain, Budget, ChannelConfig, Company, CompanyManifest, ComposioTools,
    Connection, DEFAULT_ALWAYS_APPROVE, DEFAULT_SEARCH_DAILY_CALLS, GATEABLE_NAMESPACES,
    INFERENCE_PROVIDERS, INFERENCE_TIERS, Inference, KNOWN_CHANNELS, McpServer, PLAN_NAMES,
    PLAN_PERIODS, POLICY_MODES, Place, Plan, Policy, Schedule, Skill, TIERS, TOOL_PROVIDERS, Tools,
    grants_composio_explicit, grants_media_explicit, grants_search_explicit,
    grants_workspace_write_explicit,
};
pub use workflow_file::{
    WORKFLOW_DESTINATION_KINDS, WORKFLOW_NODE_KINDS, WorkflowDestinationDef, WorkflowEdgeDef,
    WorkflowFile, WorkflowNodeDef, WorkflowNodeKind, WorkflowRetryDef, list_source_workflows,
    list_workflows_union, load_company_workflows, load_workflow_union, parse_workflow,
};
// Crate-internal only: the workflow creator (issue #69) builds a `RawWorkflow`
// from its request body, renders it to TOML, and re-parses it through
// `parse_workflow` above for validation before writing to disk.
pub(crate) use workflow_file::{RawEdge, RawNode, RawWorkflow, render_workflow};
// Crate-internal only: the shared validated-persist core (issue #112) both the
// REST `POST …/workflows` route and the orchestrator `create_workflow` tool run.
// Ungated: the REST route is in the default build, so gating this behind
// `openhuman` is what let the two surfaces drift apart (issue #168).
pub(crate) use workflow_create::{
    create_company_workflow, delete_company_workflow, seed_file_exists, update_company_workflow,
    workflow_version,
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
