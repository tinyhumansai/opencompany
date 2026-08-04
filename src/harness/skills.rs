//! The effective skill set → OpenHuman skill *read* tools + a prompt catalogue.
//!
//! A company's effective skills are the union of three sources:
//!
//! 1. **Company-dir skills** — the `SKILL.md` bundles committed under the
//!    company's source directory (`companies/<name>/skills/**`), parsed by
//!    [`load_dir_skills`](crate::company::load_dir_skills).
//! 2. **Operator deltas** — the [`SkillState`] rows the console writes through
//!    the [`SkillStateStore`](crate::ports::SkillStateStore): enable/disable
//!    overrides over a built-in, and custom skills authored in-app.
//! 3. **Custom docs** — a delta's `custom_doc` carries the full `SKILL.md` for
//!    a console-authored skill.
//!
//! [`EffectiveSkills::materialize`] folds those into one set, resolves
//! enable/disable overrides, and writes the surviving bundles into a scratch
//! `skills/<slug>/` tree under a per-agent directory. OpenHuman's three skill
//! read tools then scan that tree (its `skills/` root is the legacy skill root,
//! scanned without a trust marker) so an agent can **see and read** its skills.
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

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use openhuman_core::openhuman as oh;

use oh::config::Config;
use oh::skills::tools::{WorkflowDescribeTool, WorkflowListTool, WorkflowReadResourceTool};
use oh::tools::Tool;

use crate::company::{SkillDoc, load_dir_skills, parse_skill_md, render_skill_md};
use crate::error::OpenCompanyError;
use crate::ports::skills_state::{SkillSource, SkillState};

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
    /// `source_dir` is the company's source directory (`companies/<name>`); its
    /// `skills/` subtree supplies the committed bundles. `registry` is the
    /// repo-level shared skill library (empty in platform-provisioned mode).
    /// `deltas` are the operator overrides from the
    /// [`SkillStateStore`](crate::ports::SkillStateStore).
    ///
    /// Resolution rules:
    /// * a company-dir skill is included unless a delta disables it;
    /// * an enabled delta carrying a `custom_doc` supersedes any same-slug
    ///   company-dir body (and installs a console-authored skill outright);
    /// * a `Registry`-sourced delta whose snapshot is a pre-fix stub is healed
    ///   from `registry` — see [`is_registry_stub`];
    /// * a disabled delta drops the skill from the effective set;
    /// * a malformed `custom_doc` is skipped (never fails the build).
    ///
    /// The `workspace_dir/skills/` tree is rebuilt from scratch on every call so
    /// a rebuild reflects the current deltas (removed skills disappear).
    pub fn materialize(
        workspace_dir: PathBuf,
        source_dir: Option<&Path>,
        registry: &[SkillDoc],
        deltas: &[SkillState],
    ) -> crate::Result<Self> {
        // Parsed effective docs, and where an on-disk bundle can be copied from
        // (company-dir skills only). Custom docs carry their SKILL.md inline.
        let mut docs: BTreeMap<String, SkillDoc> = BTreeMap::new();
        let mut source_paths: BTreeMap<String, PathBuf> = BTreeMap::new();
        let mut custom_docs: BTreeMap<String, String> = BTreeMap::new();

        // 1. Company-dir skills (verbatim on-disk bundles, resources included).
        if let Some(dir) = source_dir {
            let skills_root = dir.join("skills");
            for doc in load_dir_skills(&skills_root)? {
                source_paths.insert(doc.slug.clone(), skills_root.join(&doc.slug));
                docs.insert(doc.slug.clone(), doc);
            }
        }

        // 2. Apply operator deltas: disables drop, enabled custom docs supersede.
        let mut disabled: HashSet<String> = HashSet::new();
        for delta in deltas {
            if !delta.enabled {
                disabled.insert(delta.slug.clone());
                continue;
            }
            let Some(body) = delta.custom_doc.as_deref() else {
                // An enable-only delta over a built-in: nothing to materialize
                // beyond what the company dir already supplies.
                continue;
            };
            // A pre-fix registry install snapshotted its own description as the
            // body (or, with no description to snapshot, wrote a doc that does
            // not parse at all). Either way the agent gets nothing usable, so
            // serve the live library document instead — see `registry_heal`.
            let parsed = parse_skill_md(&delta.slug, body);
            let resolved = match registry_heal(delta, parsed.as_ref().ok(), registry) {
                Some(live) => {
                    log::info!(
                        "[harness][skills] healing pre-fix registry install '{}' from the shared library",
                        delta.slug
                    );
                    Some((live.clone(), render_skill_md(live)))
                }
                None => match parsed {
                    Ok(doc) => Some((doc, body.to_string())),
                    Err(err) => {
                        log::warn!(
                            "[harness][skills] skipping malformed custom skill '{}': {err}",
                            delta.slug
                        );
                        None
                    }
                },
            };
            if let Some((doc, source)) = resolved {
                // A custom body supersedes any same-slug company-dir bundle.
                source_paths.remove(&delta.slug);
                custom_docs.insert(delta.slug.clone(), source);
                docs.insert(delta.slug.clone(), doc);
            }
        }

        // 3. Drop disabled skills from every source.
        for slug in &disabled {
            docs.remove(slug);
            source_paths.remove(slug);
            custom_docs.remove(slug);
        }

        // 4. Rebuild the scratch tree from the surviving set.
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

        for slug in docs.keys() {
            let dest = skills_out.join(slug);
            if let Some(src) = source_paths.get(slug) {
                copy_dir_recursive(src, &dest)?;
            } else if let Some(body) = custom_docs.get(slug) {
                std::fs::create_dir_all(&dest).map_err(|e| {
                    OpenCompanyError::Harness(format!("creating skill dir {}: {e}", dest.display()))
                })?;
                std::fs::write(dest.join("SKILL.md"), body).map_err(|e| {
                    OpenCompanyError::Harness(format!("writing SKILL.md for '{slug}': {e}"))
                })?;
            }
        }

        Ok(Self {
            workspace_dir,
            docs: docs.into_values().collect(),
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
    pub fn read_tools(&self) -> Vec<Box<dyn Tool>> {
        // `Config` has private fields, so build from `Default` and set the one
        // field the read tools read rather than a struct literal.
        let config = Config {
            workspace_dir: self.workspace_dir.clone(),
            ..Default::default()
        };
        let config = Arc::new(config);
        vec![
            Box::new(WorkflowListTool::new(config.clone())),
            Box::new(WorkflowDescribeTool::new(config.clone())),
            Box::new(WorkflowReadResourceTool::new(config)),
        ]
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
        out.push_str(
            "Use `list_workflows` to enumerate them, `describe_workflow` to inspect one, \
             and `read_workflow_resource` to read a skill's bundled files.\n",
        );
        out
    }

    /// The materialized skill tree's workspace dir (test/observability).
    pub fn workspace_dir(&self) -> &Path {
        &self.workspace_dir
    }
}

/// Whether a stored `SKILL.md` snapshot is a **pre-fix registry stub**.
///
/// Before this was fixed, installing a registry skill persisted a document built
/// from the client's metadata with the description doubling as the body, so the
/// agent read a one-line summary instead of the procedure. Such a snapshot is
/// recognisable by construction: its body is exactly its own description.
///
/// A legitimately one-line skill (body identical to its description) would also
/// match, and would be re-served from the live library rather than from its
/// pinned snapshot. That is the one honest false positive: it costs the pin, not
/// the content, and only for a skill whose entire body is a single line already
/// held verbatim in its own frontmatter. No skill in the shared library is
/// shaped that way (a test pins that), so it is a hypothetical.
fn is_registry_stub(doc: &SkillDoc) -> bool {
    doc.body.trim() == doc.description.trim()
}

/// The live library document that should supersede a stored snapshot, or `None`
/// to keep whatever the row has.
///
/// `stored` is the parsed snapshot, or `None` when it does not parse at all —
/// which the pre-fix path could produce, since it wrote `description:` with an
/// empty value when the client sent no description, and the parser rejects that.
/// Such a row is currently dropped from the effective set entirely, so healing it
/// turns a silently missing skill into a working one.
///
/// Scoped deliberately narrowly:
///
/// * **Only `Registry`-sourced rows.** A `Custom` row is operator-authored and a
///   `Company` row is committed to the repo; neither is ever second-guessed, so
///   the heal cannot clobber content a human wrote. There is no route that
///   writes an operator-authored body onto a `Registry` row — `install` upserts
///   a snapshot and `set_enabled` only carries the existing doc forward — so a
///   `Registry` body is always machine-generated.
/// * **Only a degenerate or unparseable snapshot.** A real snapshot is left
///   pinned, so an install does not silently track later library edits.
/// * **Only when the slug is in the library**, so an install of a skill that has
///   since left it keeps whatever it has rather than vanishing.
fn registry_heal<'a>(
    delta: &SkillState,
    stored: Option<&SkillDoc>,
    registry: &'a [SkillDoc],
) -> Option<&'a SkillDoc> {
    if delta.source != SkillSource::Registry {
        return None;
    }
    // `None` = unparseable snapshot, which is always worth replacing.
    if stored.is_some_and(|doc| !is_registry_stub(doc)) {
        return None;
    }
    registry.iter().find(|doc| doc.slug == delta.slug)
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
mod tests {
    use super::*;

    use crate::ports::skills_state::SkillSource;

    /// Writes a company-dir `skills/<slug>/SKILL.md` (plus an optional resource).
    fn seed_company_skill(source_dir: &Path, slug: &str, name: &str, resource: Option<&str>) {
        let dir = source_dir.join("skills").join(slug);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: {slug} does things\n---\n\n# {name}\n"),
        )
        .unwrap();
        if let Some(body) = resource {
            std::fs::create_dir_all(dir.join("references")).unwrap();
            std::fs::write(dir.join("references").join("spec.md"), body).unwrap();
        }
    }

    fn delta(slug: &str, enabled: bool, custom_doc: Option<&str>) -> SkillState {
        SkillState {
            slug: slug.to_string(),
            enabled,
            source: if custom_doc.is_some() {
                SkillSource::Custom
            } else {
                SkillSource::Company
            },
            custom_doc: custom_doc.map(str::to_string),
        }
    }

    /// A `Registry`-sourced delta, as `install` persists one.
    fn registry_delta(slug: &str, custom_doc: &str) -> SkillState {
        SkillState {
            slug: slug.to_string(),
            enabled: true,
            source: SkillSource::Registry,
            custom_doc: Some(custom_doc.to_string()),
        }
    }

    /// A shared-library document with a real multi-section body.
    fn library_doc(slug: &str) -> SkillDoc {
        SkillDoc {
            slug: slug.to_string(),
            name: "Competitor Scan".to_string(),
            description: "Profile competitors.".to_string(),
            category: Some("Research".to_string()),
            version: Some("1.0.0".to_string()),
            body: "\n# Competitor Scan\n\n## Steps\n\n1. Pick.\n\n## Output\n\nA table.\n"
                .to_string(),
        }
    }

    /// Exactly what the pre-fix `install` wrote: the description doubling as the
    /// body. Such a row must be re-served from the live library.
    #[test]
    fn a_pre_fix_registry_stub_is_healed_from_the_live_library() {
        let ws = tempfile::tempdir().unwrap();
        let stub = "---\nname: Competitor Scan\ndescription: Profile competitors.\ncategory: Research\n---\nProfile competitors.\n";
        let library = [library_doc("competitor-scan")];

        let eff = EffectiveSkills::materialize(
            ws.path().to_path_buf(),
            None,
            &library,
            &[registry_delta("competitor-scan", stub)],
        )
        .unwrap();

        assert_eq!(eff.docs.len(), 1);
        assert!(eff.docs[0].body.contains("## Steps"));
        assert!(eff.docs[0].body.contains("## Output"));

        // The agent reads the tree on disk, so the heal must land there too.
        let on_disk =
            std::fs::read_to_string(ws.path().join("skills/competitor-scan/SKILL.md")).unwrap();
        assert!(on_disk.contains("## Steps"), "{on_disk}");
        assert!(on_disk.contains("## Output"), "{on_disk}");
        assert!(on_disk.contains("version: 1.0.0"), "{on_disk}");
    }

    #[test]
    fn a_post_fix_registry_snapshot_is_left_pinned() {
        let ws = tempfile::tempdir().unwrap();
        // A real snapshot (body ≠ description) is never second-guessed, even
        // when the live library has since moved on.
        let pinned = "---\nname: Competitor Scan\ndescription: Profile competitors.\n---\n\n## Steps\n\n1. The pinned revision.\n";
        let mut newer = library_doc("competitor-scan");
        newer.body = "\n## Steps\n\n1. A NEWER revision.\n".to_string();

        let eff = EffectiveSkills::materialize(
            ws.path().to_path_buf(),
            None,
            &[newer],
            &[registry_delta("competitor-scan", pinned)],
        )
        .unwrap();

        assert!(
            eff.docs[0].body.contains("The pinned revision"),
            "an installed snapshot must not silently track the library"
        );
    }

    #[test]
    fn a_custom_skill_is_never_healed_even_when_its_body_is_one_line() {
        let ws = tempfile::tempdir().unwrap();
        // Same degenerate shape, but operator-authored: the heal must not touch
        // it, or console-written content would be replaced by library content.
        let authored = "---\nname: Competitor Scan\ndescription: My own note.\n---\nMy own note.\n";
        let mut delta = registry_delta("competitor-scan", authored);
        delta.source = SkillSource::Custom;

        let eff = EffectiveSkills::materialize(
            ws.path().to_path_buf(),
            None,
            &[library_doc("competitor-scan")],
            &[delta],
        )
        .unwrap();

        assert_eq!(eff.docs[0].body.trim(), "My own note.");
        assert!(!eff.docs[0].body.contains("## Steps"));
    }

    /// The pre-fix path wrote `description:` with an empty value when the client
    /// sent no description, which the parser rejects — so the row was dropped
    /// from the effective set and the agent never saw the skill at all. Once the
    /// library can serve the slug, such a row heals.
    #[test]
    fn an_unparseable_registry_snapshot_is_healed_rather_than_dropped() {
        let ws = tempfile::tempdir().unwrap();
        let broken = "---\nname: Competitor Scan\ndescription: \n---\n\n";
        assert!(
            parse_skill_md("competitor-scan", broken).is_err(),
            "this shape really is unparseable"
        );

        let eff = EffectiveSkills::materialize(
            ws.path().to_path_buf(),
            None,
            &[library_doc("competitor-scan")],
            &[registry_delta("competitor-scan", broken)],
        )
        .unwrap();

        assert_eq!(eff.docs.len(), 1, "the skill is no longer silently dropped");
        assert!(eff.docs[0].body.contains("## Steps"));
    }

    /// The same unparseable row, but for a slug the library cannot serve (a
    /// phantom the old console offered). Nothing exists to heal from, so it stays
    /// dropped — unchanged from today.
    #[test]
    fn an_unparseable_snapshot_the_library_lacks_stays_dropped() {
        let ws = tempfile::tempdir().unwrap();
        let eff = EffectiveSkills::materialize(
            ws.path().to_path_buf(),
            None,
            &[library_doc("competitor-scan")],
            &[registry_delta(
                "social-scheduler",
                "---\nname: X\ndescription: \n---\n",
            )],
        )
        .unwrap();
        assert!(eff.is_empty());
    }

    #[test]
    fn a_stub_for_a_slug_the_library_lacks_is_left_alone() {
        let ws = tempfile::tempdir().unwrap();
        let stub = "---\nname: Retired\ndescription: Gone from the library.\n---\nGone from the library.\n";

        let eff = EffectiveSkills::materialize(
            ws.path().to_path_buf(),
            None,
            &[library_doc("competitor-scan")],
            &[registry_delta("retired", stub)],
        )
        .unwrap();

        assert_eq!(
            eff.docs.len(),
            1,
            "the skill survives rather than vanishing"
        );
        assert_eq!(eff.docs[0].body.trim(), "Gone from the library.");
    }

    #[test]
    fn company_dir_skills_materialize_with_resources() {
        let src = tempfile::tempdir().unwrap();
        let ws = tempfile::tempdir().unwrap();
        seed_company_skill(src.path(), "web-research", "Web Research", Some("# spec"));

        let eff = EffectiveSkills::materialize(ws.path().to_path_buf(), Some(src.path()), &[], &[])
            .unwrap();

        // The parsed doc surfaces in the catalogue.
        assert_eq!(eff.docs.len(), 1);
        let cat = eff.catalogue();
        assert!(cat.contains("Web Research"), "{cat}");
        assert!(cat.contains("`web-research`"), "{cat}");

        // The bundle (SKILL.md + resource) is copied verbatim into the scratch.
        let out = ws.path().join("skills").join("web-research");
        assert!(out.join("SKILL.md").is_file());
        assert_eq!(
            std::fs::read_to_string(out.join("references").join("spec.md")).unwrap(),
            "# spec"
        );
    }

    #[test]
    fn disabled_delta_drops_a_company_skill() {
        let src = tempfile::tempdir().unwrap();
        let ws = tempfile::tempdir().unwrap();
        seed_company_skill(src.path(), "keep", "Keep", None);
        seed_company_skill(src.path(), "drop", "Drop", None);

        let eff = EffectiveSkills::materialize(
            ws.path().to_path_buf(),
            Some(src.path()),
            &[],
            &[delta("drop", false, None)],
        )
        .unwrap();

        let slugs: Vec<&str> = eff.docs.iter().map(|d| d.slug.as_str()).collect();
        assert_eq!(slugs, vec!["keep"]);
        assert!(!ws.path().join("skills").join("drop").exists());
        assert!(ws.path().join("skills").join("keep").exists());
    }

    #[test]
    fn custom_doc_installs_a_new_skill() {
        let ws = tempfile::tempdir().unwrap();
        let body = "---\nname: Invoicing\ndescription: Draft an invoice\n---\n\n# Invoicing\n";

        let eff = EffectiveSkills::materialize(
            ws.path().to_path_buf(),
            None,
            &[],
            &[delta("invoicing", true, Some(body))],
        )
        .unwrap();

        assert_eq!(eff.docs.len(), 1);
        assert_eq!(eff.docs[0].name, "Invoicing");
        let written =
            std::fs::read_to_string(ws.path().join("skills").join("invoicing").join("SKILL.md"))
                .unwrap();
        assert_eq!(written, body);
    }

    #[test]
    fn custom_doc_supersedes_company_body() {
        let src = tempfile::tempdir().unwrap();
        let ws = tempfile::tempdir().unwrap();
        seed_company_skill(src.path(), "report", "Old Report", None);
        let body = "---\nname: New Report\ndescription: Updated\n---\n\n# New\n";

        let eff = EffectiveSkills::materialize(
            ws.path().to_path_buf(),
            Some(src.path()),
            &[],
            &[delta("report", true, Some(body))],
        )
        .unwrap();

        assert_eq!(eff.docs.len(), 1);
        assert_eq!(eff.docs[0].name, "New Report");
        let written =
            std::fs::read_to_string(ws.path().join("skills").join("report").join("SKILL.md"))
                .unwrap();
        assert_eq!(written, body);
    }

    #[test]
    fn malformed_custom_doc_is_skipped_not_fatal() {
        let ws = tempfile::tempdir().unwrap();
        let eff = EffectiveSkills::materialize(
            ws.path().to_path_buf(),
            None,
            &[],
            &[delta("broken", true, Some("no frontmatter here"))],
        )
        .expect("malformed custom doc must not fail the build");
        assert!(eff.is_empty());
        assert!(eff.catalogue().is_empty());
    }

    #[test]
    fn empty_set_yields_no_tools_catalogue() {
        let ws = tempfile::tempdir().unwrap();
        let eff = EffectiveSkills::materialize(ws.path().to_path_buf(), None, &[], &[]).unwrap();
        assert!(eff.is_empty());
        assert!(eff.catalogue().is_empty());
    }

    #[test]
    fn read_tools_expose_three_named_tools() {
        let src = tempfile::tempdir().unwrap();
        let ws = tempfile::tempdir().unwrap();
        seed_company_skill(src.path(), "web-research", "Web Research", None);
        let eff = EffectiveSkills::materialize(ws.path().to_path_buf(), Some(src.path()), &[], &[])
            .unwrap();

        let tools = eff.read_tools();
        let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
        assert_eq!(
            names,
            vec![
                "list_workflows",
                "describe_workflow",
                "read_workflow_resource"
            ]
        );
        // The tools point at the materialized scratch dir.
        assert_eq!(eff.workspace_dir(), ws.path());
    }

    /// The console writes custom + registry skills as `SkillState` rows carrying
    /// the full `SKILL.md` inline in `custom_doc` (the registry path since PR
    /// #47). Both shapes must materialize and surface their **content** through
    /// the agent's read tools — a green build isn't enough, the body has to be
    /// readable. Also covers a frontmatter-only (empty-body) custom skill.
    #[tokio::test]
    async fn console_custom_docs_surface_content_through_read_tools() {
        use serde_json::json;

        let ws = tempfile::tempdir().unwrap();

        // Registry-install shape: source = Registry, full SKILL.md in custom_doc.
        let registry = SkillState {
            slug: "web-research".to_string(),
            enabled: true,
            source: SkillSource::Registry,
            custom_doc: Some(
                "---\nname: Web Research\ndescription: Research a topic online\n---\n\n\
                 # Web Research\n\nBODY-RESEARCH-MARKER\n"
                    .to_string(),
            ),
        };
        // Console-authored custom skill with an empty body (frontmatter only).
        let empty_body = SkillState {
            slug: "quick-note".to_string(),
            enabled: true,
            source: SkillSource::Custom,
            custom_doc: Some(
                "---\nname: Quick Note\ndescription: Jot a quick note\n---\n".to_string(),
            ),
        };

        let eff = EffectiveSkills::materialize(
            ws.path().to_path_buf(),
            None,
            &[],
            &[registry, empty_body],
        )
        .unwrap();
        assert_eq!(eff.docs.len(), 2, "both console deltas materialize");

        let tools = eff.read_tools();
        let list = tools
            .iter()
            .find(|t| t.name() == "list_workflows")
            .expect("list tool");
        let listed = list
            .execute(json!({}))
            .await
            .expect("list")
            .output_for_llm(false);
        // Both enumerate, each carrying its parsed description (content).
        assert!(listed.contains("web-research"), "{listed}");
        assert!(listed.contains("Research a topic online"), "{listed}");
        assert!(listed.contains("quick-note"), "{listed}");

        let describe = tools
            .iter()
            .find(|t| t.name() == "describe_workflow")
            .expect("describe tool");

        // The registry skill's inline body is readable — content, not just name.
        let desc = describe
            .execute(json!({ "workflow_id": "web-research" }))
            .await
            .expect("describe registry skill")
            .output_for_llm(false);
        assert!(desc.contains("BODY-RESEARCH-MARKER"), "{desc}");
        assert!(desc.contains("Research a topic online"), "{desc}");

        // The empty-body custom skill still describes cleanly (frontmatter → def).
        let desc_empty = describe
            .execute(json!({ "workflow_id": "quick-note" }))
            .await
            .expect("describe empty-body skill")
            .output_for_llm(false);
        assert!(desc_empty.contains("Jot a quick note"), "{desc_empty}");
    }

    #[tokio::test]
    async fn list_workflows_tool_sees_the_materialized_skill() {
        use serde_json::json;

        let src = tempfile::tempdir().unwrap();
        let ws = tempfile::tempdir().unwrap();
        seed_company_skill(src.path(), "web-research", "Web Research", None);
        let eff = EffectiveSkills::materialize(ws.path().to_path_buf(), Some(src.path()), &[], &[])
            .unwrap();

        let tools = eff.read_tools();
        let list = tools
            .iter()
            .find(|t| t.name() == "list_workflows")
            .expect("list tool");
        let result = list.execute(json!({})).await.expect("execute");
        let text = result.output_for_llm(false);
        // The legacy `<workspace>/skills/` root is scanned without a trust
        // marker, so the materialized bundle shows up in the tool's output.
        assert!(
            text.contains("web-research") || text.contains("Web Research"),
            "{text}"
        );
    }
}
