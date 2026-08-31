//! Agent definition files: `companies/<name>/agents/<id>.toml`.
//!
//! A company's roster may be written either inline in `company.toml` as
//! `[[agent]]` entries or — the richer form — as one file per teammate under an
//! `agents/` directory. The two are mutually exclusive per company: a bundle
//! that has both is a validation error rather than a silent precedence rule,
//! because a roster block the operator wrote and the runtime ignored is exactly
//! the failure this crate's manifest validation exists to prevent.
//!
//! The per-file form exists because a teammate is more than four fields once it
//! carries a custom prompt and its own briefing documents. Those do not fit
//! comfortably in an array-of-tables — a multi-line TOML string inside
//! `[[agent]]` is unreadable at roster length, and prose belongs beside the
//! agent it configures.
//!
//! This module parses those files, resolves each agent's
//! [`prompt_files`](crate::company::Agent::prompt_files) against the bundle, and
//! reports every problem at once in prosumer language — matching
//! [`super::manifest`] and [`super::workflow_file`].

use std::path::{Path, PathBuf};

use crate::company::Agent;
use crate::error::{OpenCompanyError, Result};

/// The bundle subdirectory holding one TOML file per roster teammate.
pub const AGENTS_DIR: &str = "agents";

/// Whether `dir` is a company bundle whose roster lives in `agents/*.toml`.
///
/// A present-but-empty `agents/` directory is **not** a bundle roster: it
/// carries no teammates, so treating it as authoritative would blank the roster
/// of a company whose `company.toml` still has a perfectly good one. An
/// unreadable directory answers `false` for the same reason — the caller then
/// parses `[[agent]]` as it always did, rather than failing the whole company
/// over a directory nothing has asked to read yet.
pub fn has_agent_files(dir: &Path) -> bool {
    !agent_file_paths(&dir.join(AGENTS_DIR)).is_empty()
}

/// Every `.toml` file directly inside `agents/`, sorted by file stem.
///
/// Sorted, not directory order, because the roster's order is load-bearing:
/// [`orchestrator_id`](crate::company::orchestrator_id) falls back to "the first
/// agent declared" when nobody is tagged `tier = "orchestrator"`, and readdir
/// order varies by filesystem. An unsorted read would make which teammate runs
/// the company depend on which machine parsed the bundle.
///
/// Only the immediate directory is read. Subdirectories are for the documents
/// `prompt_files` names, so descending into them would try to parse a `.toml`
/// briefing as a teammate.
fn agent_file_paths(agents_dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(agents_dir) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.is_file() && path.extension().is_some_and(|ext| ext == "toml"))
        .collect();
    paths.sort();
    paths
}

/// The on-disk shape of one `agents/<id>.toml`.
///
/// Every field mirrors [`Agent`], because that is what this parses into: the
/// roster type does not fork by authoring format, so a field added for the
/// bundle form is immediately available to `[[agent]]` too, and there is exactly
/// one validator and one consumer for it.
///
/// `id` is optional here and comes from the filename. It is accepted in the body
/// only as a cross-check — see [`parse_agent_file`].
#[derive(Debug, serde::Deserialize)]
struct AgentFile {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    tier: Option<String>,
    /// Which `[[harness]]` this agent runs on. Cross-checked against the
    /// company's declared harnesses in `CompanyManifest::validate`, not here —
    /// this file cannot see them.
    #[serde(default)]
    harness: Option<String>,
    /// Which model that harness should run this agent on.
    ///
    /// Cross-checked in `CompanyManifest::validate` alongside `harness`, for
    /// the same reason: a model only means something relative to the harness
    /// it is set against, and this file cannot see the company's.
    ///
    /// Absent from this struct until now, while `Agent` had the field — so a
    /// bundle whose roster lives in `agents/<id>.toml` had its `model` line
    /// dropped by serde as an unknown key and hardcoded to `None` below. That
    /// skipped validation too, so the file was neither honoured nor refused:
    /// the teammate simply ran on the harness default while its own file said
    /// otherwise.
    #[serde(default)]
    model: Option<String>,
    /// Carried verbatim onto [`Agent::tools`](crate::company::Agent::tools),
    /// whose three-state contract (issue #1804) this mirrors: an absent `tools`
    /// key parses to `None` (inherit the standard grant — every `agents/*.toml`
    /// written before #1804), `tools = []` to `Some(vec![])` (explicit deny-all),
    /// and `tools = [globs]` to `Some(globs)` (narrow).
    #[serde(default)]
    tools: Option<Vec<String>>,
    #[serde(default)]
    delegates_to: Vec<String>,
    #[serde(default)]
    context: Option<Vec<crate::company::ContextEntry>>,
    #[serde(default)]
    budget_usd_daily: Option<f64>,
    #[serde(default)]
    prompt: Option<String>,
    #[serde(default)]
    prompt_files: Vec<String>,
    #[serde(default)]
    classes: Vec<String>,
    #[serde(default)]
    ledgers: Option<Vec<crate::company::LedgerGrant>>,
    #[serde(default)]
    can_declare_ledgers: Option<bool>,
}

/// Loads every agent definition under `<dir>/agents/`, in roster order.
///
/// Field-level validity (tier names, grant shapes, `delegates_to` targets) is
/// **not** checked here: those rules are cross-cutting — a `delegates_to` entry
/// has to name a desk declared in `company.toml` — so they belong to
/// [`CompanyManifest::validate`](crate::company::CompanyManifest::validate),
/// which sees the whole company. This function is responsible only for what it
/// alone can see: that each file parses, that its identity is coherent with its
/// filename, and that the documents it names exist and can be read.
pub fn load_agents(dir: &Path) -> Result<Vec<Agent>> {
    let agents_dir = dir.join(AGENTS_DIR);
    let names: Vec<String> = agent_file_paths(&agents_dir)
        .iter()
        .filter_map(|path| path.file_name()?.to_str().map(str::to_string))
        .collect();

    load_agents_from(&agents_dir, &names, &|rel| {
        std::fs::read_to_string(agents_dir.join(rel)).map_err(|err| err.kind())
    })
}

/// [`load_agents`], reading through `read` instead of the filesystem.
///
/// A packaged desktop install carries no `companies/` directory, so its bundles
/// are embedded in the binary — and `include_str!` cannot glob, which is why
/// `build.rs` generates the table. Both callers must produce *identical*
/// rosters, so they share this function rather than each parsing agent files
/// their own way: a second parser is a second set of rules, and the roster is
/// where a silent divergence stays invisible until a teammate fails to answer.
///
/// `names` is the roster file list, already sorted — order decides which
/// teammate orchestrates when none is tagged. `read` resolves a path relative
/// to `agents/`, for both roster files and the documents `prompt_files` names.
pub(crate) fn load_agents_from(
    agents_dir: &Path,
    names: &[String],
    read: &dyn Fn(&str) -> std::result::Result<String, std::io::ErrorKind>,
) -> Result<Vec<Agent>> {
    let (agents, problems) = parse_agents(names, read);

    // Duplicate ids cannot arise from distinct filenames, but an `id` key that
    // disagrees with its stem is rejected above, so by here every id *is* its
    // stem and uniqueness is a property of the filesystem. Nothing to check.

    if problems.is_empty() {
        Ok(agents)
    } else {
        Err(OpenCompanyError::ManifestInvalid {
            path: agents_dir.to_path_buf(),
            problems,
        })
    }
}

/// Parses every named file independently, returning every agent that parsed
/// alongside every problem from the ones that did not.
///
/// [`load_agents_from`] turns this into an all-or-nothing [`Result`] for a
/// company's own roster, where one malformed file should fail the whole
/// bundle rather than silently ship a company short a teammate. The global
/// baseline (`crate::globals`) wants the opposite: a malformed *global* must
/// not cost every other global, so it calls this directly and keeps the
/// agents that parsed.
pub(crate) fn parse_agents(
    names: &[String],
    read: &dyn Fn(&str) -> std::result::Result<String, std::io::ErrorKind>,
) -> (Vec<Agent>, Vec<String>) {
    let mut agents = Vec::new();
    let mut problems = Vec::new();

    for name in names {
        match parse_agent_file(name, read) {
            Ok(agent) => agents.push(agent),
            Err(mut file_problems) => problems.append(&mut file_problems),
        }
    }

    (agents, problems)
}

/// The roster files of an embedded bundle, in the order `build.rs` recorded.
///
/// Only the immediate directory holds teammates: a `.toml` in a subdirectory is
/// a briefing document `prompt_files` names, and parsing it as an agent is
/// exactly the mistake [`agent_file_paths`] avoids on disk.
pub(crate) fn embedded_roster_names(files: &[(&str, &str)]) -> Vec<String> {
    let mut names: Vec<String> = files
        .iter()
        .map(|(name, _)| *name)
        .filter(|name| !name.contains('/') && name.ends_with(".toml"))
        .map(str::to_string)
        .collect();
    names.sort();
    names
}

/// Parses one agent file, returning every problem it has rather than the first.
fn parse_agent_file(
    name: &str,
    read: &dyn Fn(&str) -> std::result::Result<String, std::io::ErrorKind>,
) -> std::result::Result<Agent, Vec<String>> {
    let stem = name.strip_suffix(".toml").unwrap_or(name).to_string();
    let label = format!("agent file `{AGENTS_DIR}/{stem}.toml`");

    let text = read(name).map_err(|err| vec![format!("{label} could not be read — {err:?}.")])?;
    let file: AgentFile = toml::from_str(&text)
        .map_err(|err| vec![format!("{label} is not valid TOML — {}.", err.message())])?;

    let mut problems = Vec::new();

    if !super::manifest::is_snake_case(&stem) {
        problems.push(format!(
            "{label} has an invalid filename — the file name is the agent's id, so use snake_case (lowercase letters, digits, and underscores, starting with a letter)."
        ));
    }

    // An `id` key is redundant with the filename but harmless to write, so it is
    // accepted when it agrees and rejected when it does not. Silently preferring
    // one over the other would leave an operator renaming a file and wondering
    // why nothing changed — or renaming the key and wondering the same.
    if let Some(declared) = file.id.as_deref()
        && declared != stem
    {
        problems.push(format!(
            "{label} declares `id = \"{declared}\"` but its filename says `{stem}` — the filename is the id, so rename the file or drop the `id` key."
        ));
    }

    let role = file.role.unwrap_or_default();
    if role.trim().is_empty() {
        problems.push(format!("{label} is missing a `role`."));
    }

    let prompt_files_resolved = match resolve_prompt_files(read, &label, &file.prompt_files) {
        Ok(resolved) => resolved,
        Err(mut file_problems) => {
            problems.append(&mut file_problems);
            Vec::new()
        }
    };

    if !problems.is_empty() {
        return Err(problems);
    }

    Ok(Agent {
        id: stem,
        role,
        description: file.description,
        tier: file.tier,
        harness: file.harness,
        tools: file.tools,
        delegates_to: file.delegates_to,
        context: file.context,
        budget_usd_daily: file.budget_usd_daily,
        prompt: file.prompt,
        prompt_files: file.prompt_files,
        prompt_files_resolved,
        classes: file.classes,
        ledgers: file.ledgers,
        name: None,
        can_declare_ledgers: file.can_declare_ledgers.unwrap_or(true),
        // Provenance is set by whoever merges the baseline in, never by a file:
        // this same parser reads both a company's `agents/` and `globals/`.
        global: false,
        model: file.model,
    })
}

/// Reads each `prompt_files` entry relative to `agents/`, refusing any path that
/// leaves that directory.
///
/// The traversal check is done on the **path components**, before touching the
/// filesystem, rather than by canonicalizing and comparing prefixes. Canonical
/// comparison would resolve symlinks, which makes whether a bundle is valid
/// depend on how the checkout was laid out on the reading machine; a company
/// that parses on one host must parse on every host. Absolute paths and `..`
/// are both rejected outright — an agent's briefing lives beside the agent.
fn resolve_prompt_files(
    read: &dyn Fn(&str) -> std::result::Result<String, std::io::ErrorKind>,
    label: &str,
    entries: &[String],
) -> std::result::Result<Vec<(String, String)>, Vec<String>> {
    let mut resolved = Vec::new();
    let mut problems = Vec::new();

    for entry in entries {
        let rel = Path::new(entry);
        let escapes = rel.is_absolute()
            || rel
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir));
        if escapes {
            problems.push(format!(
                "{label} names `prompt_files` entry `{entry}`, which points outside `{AGENTS_DIR}/` — a prompt document must live beside the agent that uses it."
            ));
            continue;
        }

        match read(entry) {
            Ok(body) => resolved.push((entry.clone(), body)),
            Err(std::io::ErrorKind::NotFound) => problems.push(format!(
                "{label} names `prompt_files` entry `{entry}`, which does not exist — create `{AGENTS_DIR}/{entry}` or remove the entry."
            )),
            Err(err) => problems.push(format!(
                "{label} could not read `prompt_files` entry `{entry}` — {err:?}."
            )),
        }
    }

    if problems.is_empty() {
        Ok(resolved)
    } else {
        Err(problems)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Writes a bundle with the given `agents/` files and returns its root.
    fn bundle(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        let agents = dir.path().join(AGENTS_DIR);
        std::fs::create_dir_all(&agents).expect("agents dir");
        for (name, body) in files {
            let path = agents.join(name);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).expect("parent dir");
            }
            std::fs::write(path, body).expect("write");
        }
        dir
    }

    fn problems_of(err: crate::error::OpenCompanyError) -> Vec<String> {
        match err {
            crate::error::OpenCompanyError::ManifestInvalid { problems, .. } => problems,
            other => panic!("expected ManifestInvalid, got {other}"),
        }
    }

    #[test]
    fn an_absent_or_empty_agents_directory_is_not_a_bundle_roster() {
        let empty = tempfile::tempdir().expect("tempdir");
        assert!(
            !has_agent_files(empty.path()),
            "a bundle with no `agents/` at all keeps its `[[agent]]` roster"
        );

        // Present but empty: still not a roster. Treating it as one would blank
        // the roster of a company whose `company.toml` has a perfectly good one.
        let dir = bundle(&[]);
        assert!(!has_agent_files(dir.path()));
    }

    #[test]
    fn loads_agents_sorted_by_stem_not_directory_order() {
        // Written in an order that is not sorted, so a readdir-order
        // implementation would visibly disagree. Roster order decides which
        // teammate is the orchestrator when nobody is tagged, so this is
        // load-bearing rather than cosmetic.
        let dir = bundle(&[
            ("zara.toml", "role = \"Zara\"\n"),
            ("alice.toml", "role = \"Alice\"\n"),
            ("mike.toml", "role = \"Mike\"\n"),
        ]);

        let agents = load_agents(dir.path()).expect("loads");
        let ids: Vec<&str> = agents.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(ids, ["alice", "mike", "zara"]);
    }

    #[test]
    fn the_filename_is_the_id_and_a_declared_id_must_agree() {
        let dir = bundle(&[("copywriter.toml", "role = \"Copywriter\"\n")]);
        let agents = load_agents(dir.path()).expect("loads");
        assert_eq!(agents[0].id, "copywriter");

        let dir = bundle(&[(
            "copywriter.toml",
            "id = \"copy_writer\"\nrole = \"Copywriter\"\n",
        )]);
        let problems = problems_of(load_agents(dir.path()).expect_err("mismatched id is refused"));
        assert_eq!(problems.len(), 1);
        // The message must name both halves: an operator who renamed one of them
        // needs to know which one the runtime believed.
        assert!(problems[0].contains("copy_writer"), "{problems:?}");
        assert!(problems[0].contains("copywriter"), "{problems:?}");
    }

    #[test]
    fn a_non_snake_case_filename_is_refused() {
        let dir = bundle(&[("CopyWriter.toml", "role = \"Copywriter\"\n")]);
        let problems = problems_of(load_agents(dir.path()).expect_err("refused"));
        assert!(
            problems[0].contains("snake_case"),
            "the message must say what shape is wanted: {problems:?}"
        );
    }

    #[test]
    fn a_missing_role_is_refused() {
        let dir = bundle(&[("copywriter.toml", "description = \"writes\"\n")]);
        let problems = problems_of(load_agents(dir.path()).expect_err("refused"));
        assert!(problems[0].contains("`role`"), "{problems:?}");
    }

    #[test]
    fn every_problem_across_every_file_is_reported_at_once() {
        // Two broken files, two distinct problems in the second. An operator
        // fixing one problem per run is the failure this collects to avoid.
        let dir = bundle(&[
            ("Bad_Name.toml", "role = \"Fine\"\n"),
            (
                "other.toml",
                "role = \"\"\nprompt_files = [\"nope.md\", \"../escape.md\"]\n",
            ),
        ]);
        let problems = problems_of(load_agents(dir.path()).expect_err("refused"));
        assert_eq!(problems.len(), 4, "{problems:?}");
    }

    #[test]
    fn prompt_files_are_read_from_the_bundle() {
        let dir = bundle(&[
            (
                "copywriter.toml",
                "role = \"Copywriter\"\nprompt_files = [\"prompts/tone.md\"]\n",
            ),
            ("prompts/tone.md", "Lead with the reader's problem."),
        ]);

        let agents = load_agents(dir.path()).expect("loads");
        assert_eq!(
            agents[0].prompt_files_resolved,
            vec![(
                "prompts/tone.md".to_string(),
                "Lead with the reader's problem.".to_string()
            )]
        );
        // The declared list survives alongside the resolved bodies: the console
        // renders what the agent asked for, not just what it got.
        assert_eq!(agents[0].prompt_files, vec!["prompts/tone.md".to_string()]);
    }

    #[test]
    fn a_missing_prompt_file_is_an_error_rather_than_a_skipped_entry() {
        // The whole point: a typo here would otherwise yield a role whose prompt
        // was written around a briefing it silently never received.
        let dir = bundle(&[(
            "copywriter.toml",
            "role = \"Copywriter\"\nprompt_files = [\"prompts/tone.md\"]\n",
        )]);
        let problems = problems_of(load_agents(dir.path()).expect_err("refused"));
        assert!(problems[0].contains("prompts/tone.md"), "{problems:?}");
        assert!(problems[0].contains("does not exist"), "{problems:?}");
    }

    #[test]
    fn a_prompt_file_path_may_not_escape_the_agents_directory() {
        for escape in ["../secrets.md", "nested/../../secrets.md", "/etc/passwd"] {
            let dir = bundle(&[(
                "copywriter.toml",
                &format!("role = \"Copywriter\"\nprompt_files = [\"{escape}\"]\n"),
            )]);
            let problems = problems_of(
                load_agents(dir.path()).unwrap_err_or_else_panic(&format!("{escape} is refused")),
            );
            assert!(problems[0].contains("outside"), "{escape} → {problems:?}");
        }
    }

    /// A per-file teammate's `model` is carried, like its `harness`.
    ///
    /// `AgentFile` had `harness` but not `model`, so serde dropped the line as
    /// an unknown key and the built `Agent` hardcoded `None`. The failure was
    /// silent in both directions: the override never applied, and because
    /// `CompanyManifest::validate` only sees what parsing produced, the file
    /// was not refused either. A bundle could state a model, be accepted, and
    /// run on the harness default.
    #[test]
    fn a_per_file_teammate_carries_its_model_override() {
        let dir = bundle(&[(
            "critic.toml",
            "role = \"Critic\"\nharness = \"laptop\"\nmodel = \"claude-opus-4-5\"\n",
        )]);
        let agents = load_agents(dir.path()).expect("loads");
        let critic = agents.iter().find(|a| a.id == "critic").expect("parsed");
        assert_eq!(critic.harness.as_deref(), Some("laptop"));
        assert_eq!(
            critic.model.as_deref(),
            Some("claude-opus-4-5"),
            "a model in the file must reach the agent, or it is neither honoured nor refused"
        );
    }

    #[test]
    fn a_subdirectory_toml_is_a_document_not_a_teammate() {
        // `prompt_files` may point at a `.toml` briefing; descending into
        // subdirectories would try to parse it as a roster entry.
        let dir = bundle(&[
            ("copywriter.toml", "role = \"Copywriter\"\n"),
            ("prompts/reference.toml", "not = \"an agent\"\n"),
        ]);
        let agents = load_agents(dir.path()).expect("loads");
        let ids: Vec<&str> = agents.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(ids, ["copywriter"]);
    }

    #[test]
    fn the_full_enriched_shape_round_trips() {
        let dir = bundle(&[
            (
                "critic.toml",
                r#"
role = "Critic"
description = "Challenge a deliverable."
tier = "reasoning"
tools = ["docs.*", "mcp:notion"]
delegates_to = ["research"]
budget_usd_daily = 5.0
prompt = "Be specific about what would change your mind."
prompt_files = ["prompts/rubric.md"]
context = ["GOAL.md", "claims.md"]
classes = ["judge", "evidence"]
"#,
            ),
            ("prompts/rubric.md", "Score against the brief."),
        ]);

        let agent = load_agents(dir.path()).expect("loads").remove(0);
        assert_eq!(agent.id, "critic");
        assert_eq!(agent.tier.as_deref(), Some("reasoning"));
        assert_eq!(
            agent.tools,
            Some(vec!["docs.*".to_string(), "mcp:notion".to_string()])
        );
        assert_eq!(agent.delegates_to, ["research"]);
        assert_eq!(agent.budget_usd_daily, Some(5.0));
        assert_eq!(
            agent.prompt.as_deref(),
            Some("Be specific about what would change your mind.")
        );
        assert_eq!(
            agent.context.as_deref(),
            Some(
                &[
                    crate::company::ContextEntry::from("GOAL.md"),
                    crate::company::ContextEntry::from("claims.md")
                ][..]
            )
        );
        assert_eq!(agent.classes, ["judge", "evidence"]);
    }

    /// `context = []` must survive as an explicit empty list, distinct from an
    /// omitted key — the routing layer reads the two differently.
    #[test]
    fn an_explicit_empty_context_is_distinct_from_an_omitted_one() {
        let dir = bundle(&[
            ("omitted.toml", "role = \"A\"\n"),
            ("explicit.toml", "role = \"B\"\ncontext = []\n"),
        ]);
        let agents = load_agents(dir.path()).expect("loads");
        let explicit = agents.iter().find(|a| a.id == "explicit").expect("found");
        let omitted = agents.iter().find(|a| a.id == "omitted").expect("found");
        assert_eq!(explicit.context, Some(Vec::new()));
        assert_eq!(omitted.context, None);
    }

    /// Small helper so the escape loop above reads as one assertion per case.
    trait UnwrapErrOrPanic<T> {
        fn unwrap_err_or_else_panic(self, message: &str) -> crate::error::OpenCompanyError;
    }

    impl<T> UnwrapErrOrPanic<T> for Result<T> {
        fn unwrap_err_or_else_panic(self, message: &str) -> crate::error::OpenCompanyError {
            match self {
                Ok(_) => panic!("{message}"),
                Err(err) => err,
            }
        }
    }
}
