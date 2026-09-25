//! The effective skill set → OpenHuman skill *read* tools + a prompt catalogue.
//!
//! What a company's effective skills *are* lives in
//! [`crate::company::skill_effective`], which the console's read paths share.
//! [`EffectiveSkills::materialize`] takes that set and writes its enabled
//! entries into a scratch `skills/<slug>/` tree under a per-agent directory.
//! OpenHuman's three skill read tools then scan that tree (its `skills/` root is
//! the legacy skill root, scanned without a trust marker) so an agent can **see
//! and read** its skills.
//!
//! ## Freshness
//!
//! The effective set is recomputed from the current deltas on **every** build,
//! and the scratch tree is rebuilt from scratch each time (a dropped skill
//! disappears). The harness re-drives this whenever the operator's deltas move:
//! [`HarnessPool::ensure`](crate::harness::HarnessPool::ensure) fetches the
//! deltas at the top of each cycle and rebuilds the roster when they differ, so
//! a skill authored / enabled / disabled in the console surfaces to every agent
//! on the next cycle — no process restart. An unchanged delta set is a no-op:
//! the cached roster (and each agent's conversation state) is left in place.
//!
//! This is deliberately **read-only**: skill *execution* (`run_workflow`) is not
//! wired here. `RunWorkflowTool` reaches for the global `Config::load_or_init()`
//! and bypasses the harness's metering, so it needs an upstream injection seam
//! that does not exist yet — it is out of scope for this slice.
//!
//! Compiled only under `feature = "openhuman"` (the whole `harness` module is).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use openhuman_core as oh;

use oh::config::Config;
use oh::skills::tools::{WorkflowDescribeTool, WorkflowListTool, WorkflowReadResourceTool};
use tinytools::Tool;

use crate::company::SkillDoc;
use crate::company::skill_effective::{self, SkillBody};
use crate::error::OpenCompanyError;
use crate::ports::skills_state::SkillState;

mod naming;

pub use naming::{DESCRIBE_SKILL_TOOL, LIST_SKILLS_TOOL, READ_SKILL_RESOURCE_TOOL};

/// One agent's effective, enabled skill set, materialized on disk so OpenHuman's
/// skill read tools can scan it.
pub struct EffectiveSkills {
    /// The read-tools' workspace dir. Its `skills/<slug>/SKILL.md` tree holds the
    /// materialized effective set; a synthesized [`Config`] points OpenHuman's
    /// read tools at it.
    workspace_dir: PathBuf,
    /// The enabled effective skill docs, ordered by slug.
    docs: Vec<SkillDoc>,
}

impl EffectiveSkills {
    /// Materializes the effective skill set for one agent under `workspace_dir`.
    ///
    /// The set itself is resolved by
    /// [`skill_effective::resolve_for_agent`](crate::company::skill_effective::resolve_for_agent),
    /// a narrowing of the [`resolve`](crate::company::skill_effective::resolve)
    /// the console's two read paths share — so what an agent gets on disk and
    /// what the Skills tab reports are the same derivation. This writes the
    /// enabled entries out; a disabled one is reported by the readers and never
    /// materialized.
    ///
    /// `agent` is the teammate's id, carried through so the warning
    /// `resolve_for_agent` raises over a scope entry the company does not have
    /// enabled names which teammate's scope it came from.
    ///
    /// `agent_skills` is the teammate's own scope, and it is applied **before**
    /// anything is written. An unlisted skill never reaches this tree, so the
    /// catalogue and the three read tools — which are derived from the tree and
    /// nothing else — cannot disagree with it. Trimming the catalogue instead
    /// would leave `read_skill_resource` able to open a skill the agent does not
    /// have.
    ///
    /// The `workspace_dir/skills/` tree is rebuilt from scratch on every call so
    /// a rebuild reflects the current deltas (removed skills disappear).
    pub fn materialize(
        workspace_dir: PathBuf,
        source_dir: Option<&Path>,
        registry: &[SkillDoc],
        deltas: &[SkillState],
        agent: &str,
        agent_skills: Option<&[String]>,
    ) -> crate::Result<Self> {
        let effective =
            skill_effective::resolve_for_agent(source_dir, registry, deltas, agent, agent_skills)?;

        let skills_out = workspace_dir.join("skills");
        if skills_out.exists() {
            std::fs::remove_dir_all(&skills_out).map_err(|e| {
                OpenCompanyError::Harness(format!(
                    "clearing skill scratch {}: {e}",
                    skills_out.display()
                ))
            })?;
        }
        std::fs::create_dir_all(&skills_out).map_err(|e| {
            OpenCompanyError::Harness(format!(
                "creating skill scratch {}: {e}",
                skills_out.display()
            ))
        })?;

        let mut docs = Vec::new();
        for skill in effective {
            if !skill.enabled {
                continue;
            }
            let Some(content) = skill.content else {
                continue;
            };
            let dest = skills_out.join(&skill.slug);
            match &content.body {
                SkillBody::Bundle(src) => copy_dir_recursive(src, &dest)?,
                SkillBody::Inline(body) => {
                    std::fs::create_dir_all(&dest).map_err(|e| {
                        OpenCompanyError::Harness(format!(
                            "creating skill dir {}: {e}",
                            dest.display()
                        ))
                    })?;
                    std::fs::write(dest.join("SKILL.md"), body).map_err(|e| {
                        OpenCompanyError::Harness(format!(
                            "writing SKILL.md for '{}': {e}",
                            skill.slug
                        ))
                    })?;
                }
            }
            docs.push(content.doc);
        }

        Ok(Self {
            workspace_dir,
            docs,
        })
    }

    /// Whether the effective set is empty (no skills to surface).
    pub fn is_empty(&self) -> bool {
        self.docs.is_empty()
    }

    /// The three OpenHuman skill **read** tools, scoped to this agent's
    /// materialized skill tree.
    ///
    /// Each tool consumes only `config.workspace_dir` (verified upstream), so a
    /// throwaway [`Config`] with just that field set is enough — the global
    /// `Config::load_or_init()` and its registry are never booted.
    ///
    /// Wrapped by [`naming::skill_read_tools`] so they are named, described,
    /// parameterized and answered in terms of **skills** (issue #845). Upstream
    /// calls a skill a "workflow", which is a different thing entirely in a host
    /// that has a workflow registry of its own — unrenamed, `list_workflows`
    /// answered a question about the company's workflows with the contents of
    /// `Settings → Skills`. See [`naming`] for why the rename is not only the
    /// tool name.
    pub fn read_tools(&self) -> Vec<Box<dyn Tool>> {
        // `Config` has private fields, so build from `Default` and set the one
        // field the read tools read rather than a struct literal.
        let config = Config {
            workspace_dir: self.workspace_dir.clone(),
            ..Default::default()
        };
        let config = Arc::new(config);
        naming::skill_read_tools(
            Box::new(WorkflowListTool::new(config.clone())),
            Box::new(WorkflowDescribeTool::new(config.clone())),
            Box::new(WorkflowReadResourceTool::new(config)),
        )
    }

    /// A plain-text catalogue of the effective skills for the persona prompt.
    ///
    /// Returns an empty string when the set is empty so an agent with no skills
    /// gets no catalogue (and the persona is left untouched). The catalogue is
    /// folded into the persona body — `SystemPromptBuilder::for_subagent`'s
    /// `omit_skills_catalog` flag is inert upstream, so it cannot be relied on.
    pub fn catalogue(&self) -> String {
        if self.docs.is_empty() {
            return String::new();
        }
        let mut out = String::from(
            "\n\nSkills available to you (read-only). Each is a packaged, reusable \
             procedure:\n",
        );
        for doc in &self.docs {
            out.push_str(&format!(
                "- {} (`{}`): {}\n",
                doc.name, doc.slug, doc.description
            ));
        }
        // Named after skills, like the tools themselves (issue #845). This
        // sentence is what hands an agent the three names, so it is also what
        // taught every agent to call a skill a workflow.
        out.push_str(&format!(
            "Use `{LIST_SKILLS_TOOL}` to enumerate them, `{DESCRIBE_SKILL_TOOL}` to inspect \
             one, and `{READ_SKILL_RESOURCE_TOOL}` to read a skill's bundled files. A skill is \
             not one of the company's saved workflows — those are stored graphs, listed on the \
             Workflows page, and none of these three tools can see them.\n",
        ));
        out
    }

    /// The materialized skill tree's workspace dir (test/observability).
    pub fn workspace_dir(&self) -> &Path {
        &self.workspace_dir
    }
}

/// Recursively copies a skill bundle directory (SKILL.md plus any bundled
/// resource files) into `dest`. Regular files and directories only — symlinks
/// are skipped so a bundle can't smuggle out-of-tree content into the scratch.
fn copy_dir_recursive(src: &Path, dest: &Path) -> crate::Result<()> {
    std::fs::create_dir_all(dest)
        .map_err(|e| OpenCompanyError::Harness(format!("creating {}: {e}", dest.display())))?;
    let entries = std::fs::read_dir(src)
        .map_err(|e| OpenCompanyError::Harness(format!("reading {}: {e}", src.display())))?;
    for entry in entries {
        let entry = entry
            .map_err(|e| OpenCompanyError::Harness(format!("reading {}: {e}", src.display())))?;
        let file_type = entry.file_type().map_err(|e| {
            OpenCompanyError::Harness(format!("stat {}: {e}", entry.path().display()))
        })?;
        if file_type.is_symlink() {
            continue;
        }
        let from = entry.path();
        let to = dest.join(entry.file_name());
        if file_type.is_dir() {
            copy_dir_recursive(&from, &to)?;
        } else if file_type.is_file() {
            std::fs::copy(&from, &to).map_err(|e| {
                OpenCompanyError::Harness(format!(
                    "copying {} -> {}: {e}",
                    from.display(),
                    to.display()
                ))
            })?;
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "skills_scope_tests.rs"]
mod scope_tests;
#[cfg(test)]
#[path = "skills_tests.rs"]
mod tests;
