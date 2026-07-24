//! Company definition: the on-disk manifest and the entrypoints that load it.
//!
//! Phase 0 of the runtime: parse and validate `company.toml` / `agents.toml`,
//! surface problems in prosumer language, and boot a company far enough to
//! report its effective configuration. The cognition kernel (Brain, cycle
//! loop, stores) lands in later phases; see `docs/spec/roadmap.md`.

#[cfg(test)]
mod content_test;
pub mod dns;
pub mod inference;
mod manifest;
pub mod mcp;
pub mod runtime;
mod skill_file;
pub mod telegram;
mod types;
mod workflow_file;
pub mod workspace_seed;

use std::path::Path;

pub use manifest::{LEGACY_MANIFEST_FILE, Located, MANIFEST_FILE, discover};
pub use skill_file::{SkillDoc, load_dir_skills, parse_skill_md};
pub use types::{
    Agent, BRAIN_MODES, Brain, Budget, ChannelConfig, Company, CompanyManifest, Connection,
    DEFAULT_ALWAYS_APPROVE, INFERENCE_PROVIDERS, INFERENCE_TIERS, Inference, KNOWN_CHANNELS,
    McpServer, POLICY_MODES, Place, Policy, Schedule, Skill, TIERS, TOOL_PROVIDERS, Tools,
};
pub use workflow_file::{
    WORKFLOW_NODE_KINDS, WorkflowEdgeDef, WorkflowFile, WorkflowNodeDef, WorkflowNodeKind,
    load_company_workflows, parse_workflow,
};
// Crate-internal only: the workflow creator (issue #69) builds a `RawWorkflow`
// from its request body, renders it to TOML, and re-parses it through
// `parse_workflow` above for validation before writing to disk.
pub(crate) use workflow_file::{RawEdge, RawNode, RawWorkflow, render_workflow};
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
