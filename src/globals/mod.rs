//! The global baseline: what every company has, whichever vertical it started
//! from.
//!
//! A `companies/<name>/` bundle describes one vertical. This module is the part
//! that is the same in all of them — a small roster, two workflow graphs, a few
//! installed skills, and the tool belt a company starts from.
//! The authored sources live in `globals/` beside `companies/`, and the
//! contract is `docs/spec/runtime/globals.md`.
//!
//! Everything here is **embedded at build time**, not read from disk. A
//! platform-provisioned tenant container carries no repository checkout — the
//! same reason `skills_root()` is `None` there and the shared skill registry is
//! empty — so a baseline resolved from the filesystem would be a baseline every
//! hosted company silently lacked. `build.rs` generates the tables; this module
//! parses them once and caches the result.
//!
//! Malformed input is a fault, never a panic and never an abort: a company must
//! still reach the rest of its baseline when one global is broken, exactly as
//! [`crate::ledger::registry::builtins`] treats a malformed built-in ledger.
//!
//! # Precedence
//!
//! A company always wins. Every merge point resolves an id collision toward the
//! company's own definition and never merges field-by-field:
//!
//! * roster — [`crate::company::CompanyManifest`] appends globals *after* the
//!   company's own agents, skipping any id the company already declares;
//! * workflows — the company's `workflows/<id>.toml`, then its console-authored
//!   overlay, then the global graph;
//! * skills — a company-bundle or console-authored skill of the same slug
//!   supersedes the installed global one;
//! * tools — `[tools].default_allow` is the belt a company with no `[tools]`
//!   section gets; one that declares grants keeps exactly what it declared.
//!
//! A company drops a global outright with `[globals].disable`, whose entries are
//! `<kind>:<id>` keys validated against what this module actually carries.

use std::sync::OnceLock;

use crate::company::{Agent, SkillDoc, WorkflowFile};
use crate::ledger::LedgerSpec;

mod generated {
    include!(concat!(env!("OUT_DIR"), "/embedded_globals.rs"));
}

#[cfg(test)]
mod test;

/// The `<kind>` half of a `[globals].disable` entry.
///
/// Closed, and each variant maps to exactly one surface below. A free-form kind
/// would let a typo (`agents:researcher`) read as a disable that never fires,
/// which is the one failure mode an opt-out list must not have.
pub const DISABLE_KINDS: &[&str] = &["agent", "workflow", "skill", "ledger", "task"];

/// The parsed `globals/globals.toml`.
#[derive(Debug, Default, serde::Deserialize)]
struct GlobalsManifest {
    #[serde(default)]
    tools: GlobalTools,
    #[serde(default)]
    skills: GlobalSkills,
}

#[derive(Debug, Default, serde::Deserialize)]
struct GlobalTools {
    #[serde(default)]
    default_allow: Vec<String>,
}

#[derive(Debug, Default, serde::Deserialize)]
struct GlobalSkills {
    #[serde(default)]
    always: Vec<String>,
}

/// Everything the baseline carries, parsed once.
#[derive(Debug, Default)]
struct Baseline {
    agents: Vec<Agent>,
    workflows: Vec<WorkflowFile>,
    ledgers: Vec<LedgerSpec>,
    tasks: Vec<crate::company::TaskSeed>,
    skills: Vec<SkillDoc>,
    default_tool_allow: Vec<String>,
    faults: Vec<String>,
}

fn baseline() -> &'static Baseline {
    static BASELINE: OnceLock<Baseline> = OnceLock::new();
    BASELINE.get_or_init(build)
}

fn build() -> Baseline {
    let mut faults = Vec::new();

    let manifest: GlobalsManifest = match toml::from_str(generated::EMBEDDED_GLOBALS_MANIFEST) {
        Ok(manifest) => manifest,
        Err(err) => {
            faults.push(format!("`globals/globals.toml` is not valid TOML — {err}"));
            GlobalsManifest::default()
        }
    };

    Baseline {
        agents: build_agents(&mut faults),
        workflows: build_workflows(&mut faults),
        ledgers: build_ledgers(&mut faults),
        tasks: build_tasks(&mut faults),
        skills: build_skills(&manifest.skills.always, &mut faults),
        default_tool_allow: manifest.tools.default_allow,
        faults,
    }
}

fn build_agents(faults: &mut Vec<String>) -> Vec<Agent> {
    let files = generated::EMBEDDED_GLOBAL_AGENTS;
    let names = crate::company::agent_file::embedded_roster_names(files);
    agents_from(
        &names,
        &|rel| {
            files
                .iter()
                .find(|(name, _)| *name == rel)
                .map(|(_, body)| (*body).to_string())
                .ok_or(std::io::ErrorKind::NotFound)
        },
        faults,
    )
}

/// [`build_agents`], reading through `read` instead of the embedded table —
/// separated out so a test can hand it one malformed file and one valid file
/// without touching the shipped baseline.
///
/// Parses each file independently via
/// [`parse_agents`](crate::company::agent_file::parse_agents) rather than
/// [`load_agents_from`](crate::company::agent_file::load_agents_from)'s
/// all-or-nothing [`Result`]: a company's roster fails the whole bundle on one
/// bad file, but the global baseline must keep every global that *did* parse
/// — the fault-isolation contract this module's docs promise.
fn agents_from(
    names: &[String],
    read: &dyn Fn(&str) -> std::result::Result<String, std::io::ErrorKind>,
    faults: &mut Vec<String>,
) -> Vec<Agent> {
    let (agents, problems) = crate::company::agent_file::parse_agents(names, read);
    faults.extend(
        problems
            .into_iter()
            .map(|problem| format!("a global agent is malformed: {problem}")),
    );

    agents
        .into_iter()
        .filter(|agent| {
            // A global tagged `orchestrator` would take the company's own
            // roster's job in every company at once, since `orchestrator_id`
            // prefers a tagged agent over declaration order. Refused here
            // rather than trusted to review.
            let tagged = agent.tier.as_deref() == Some(crate::company::ORCHESTRATOR_TIER);
            if tagged {
                faults.push(format!(
                    "global agent `{}` is tagged `tier = \"orchestrator\"` — a global teammate cannot orchestrate a company it has never seen, so it is dropped.",
                    agent.id
                ));
            }
            !tagged
        })
        .collect()
}

fn build_workflows(faults: &mut Vec<String>) -> Vec<WorkflowFile> {
    let mut out = Vec::new();
    for (name, body) in generated::EMBEDDED_GLOBAL_WORKFLOWS {
        if name.contains('/') || !name.ends_with(".toml") {
            continue;
        }
        match crate::company::parse_workflow(body) {
            Ok(workflow) => out.push(workflow),
            Err(err) => faults.push(format!("global workflow `{name}` is malformed: {err}")),
        }
    }
    out
}

/// The baseline's own ledgers, parsed with the same fault isolation the roster
/// gets: one malformed declaration costs itself and nothing beside it.
///
/// These are axes every company has whatever it sells — not the built-ins,
/// which the runtime ships in Rust and no file can shadow, but the ones a
/// company would otherwise have to think of declaring before it could record
/// anything on them.
fn build_ledgers(faults: &mut Vec<String>) -> Vec<LedgerSpec> {
    let files = generated::EMBEDDED_GLOBAL_LEDGERS;
    let names = crate::company::ledger_file::embedded_ledger_names(files);
    let (specs, problems) = crate::company::ledger_file::parse_ledgers(&names, &|rel| {
        files
            .iter()
            .find(|(name, _)| *name == rel)
            .map(|(_, body)| (*body).to_string())
            .ok_or(std::io::ErrorKind::NotFound)
    });
    faults.extend(
        problems
            .into_iter()
            .map(|problem| format!("a global ledger is malformed: {problem}")),
    );
    specs
}

/// Parses `globals/tasks.toml` into the baseline's seed cards.
///
/// Fault-isolated like every other global surface: a malformed baseline costs
/// its own cards and nothing else, because the alternative is one bad line
/// stopping every company on the install from booting.
fn build_tasks(faults: &mut Vec<String>) -> Vec<crate::company::TaskSeed> {
    let (tasks, problems) = crate::company::task_file::parse_tasks(
        "globals/tasks.toml",
        generated::EMBEDDED_GLOBAL_TASKS,
    );
    faults.extend(
        problems
            .into_iter()
            .map(|problem| format!("a global seed card is malformed: {problem}")),
    );
    tasks
}

fn build_skills(always: &[String], faults: &mut Vec<String>) -> Vec<SkillDoc> {
    let mut out = Vec::new();
    for slug in always {
        let Some((_, body)) = generated::EMBEDDED_SHARED_SKILLS
            .iter()
            .find(|(candidate, _)| candidate == slug)
        else {
            faults.push(format!(
                "`[skills].always` names `{slug}`, which is not in the shared library (`skills/{slug}/SKILL.md`)."
            ));
            continue;
        };
        match crate::company::parse_skill_md(slug, body) {
            Ok(doc) => out.push(doc),
            Err(err) => faults.push(format!("global skill `{slug}` is malformed: {err}")),
        }
    }
    out
}

/// The global roster, in filename order, never including an orchestrator.
pub fn agents() -> &'static [Agent] {
    &baseline().agents
}

/// The global workflow graphs, in filename order.
pub fn workflows() -> &'static [WorkflowFile] {
    &baseline().workflows
}

/// The ledgers every company gets on top of the runtime's built-ins.
///
/// Seeded into a company's store at first boot alongside whatever its own
/// bundle declares, and never re-seeded — retiring one is a person's call, and
/// a baseline that re-asserted itself on the next restart would take that call
/// back. See `runtime::builder`.
pub fn ledgers() -> &'static [LedgerSpec] {
    &baseline().ledgers
}

/// The setup cards every company starts with on its board.
///
/// Seeded once, at first boot, alongside whatever its own bundle seeds — and
/// never re-seeded, for the reason [`ledgers`] is not: clearing a card is a
/// person's call, and a baseline that re-asserted it on the next restart would
/// take that call back. Clearing the board is routine, so this matters more
/// here than it does for a ledger. See `runtime::builder`.
pub fn tasks() -> &'static [crate::company::TaskSeed] {
    &baseline().tasks
}

/// The shared-library skills installed in every company.
pub fn skills() -> &'static [SkillDoc] {
    &baseline().skills
}

/// The tool belt a company gets when its manifest declares no `[tools]` section.
///
/// Deliberately a **default**, not a floor. A floor would grant namespaces on
/// top of whatever a company allowed, and so re-grant authority a company
/// withheld on purpose — `workspace` confers workspace writes that not even a
/// `*` grant does, and an agent with no `files` grant is meant to be offered no
/// file tools. What is global here is where the starting belt is *authored*, not
/// a minimum nobody can go under.
/// Falls back to the authored default belt if the baseline carries none, so a
/// missing or malformed `globals.toml` cannot leave every company grantless —
/// the failure mode of a data-driven default has to preserve the shipped
/// capability contract, not silently remove search, MCP, or workspace writes.
pub fn default_tool_allow() -> Vec<String> {
    let authored = &baseline().default_tool_allow;
    if authored.is_empty() {
        return vec![
            "*".into(),
            "workspace.*".into(),
            "workspace.write".into(),
            "media".into(),
            "composio".into(),
            "search".into(),
            "mcp:*".into(),
        ];
    }
    authored.clone()
}

/// Every problem found while parsing the baseline — empty in a healthy build.
///
/// Surfaced rather than swallowed so a broken global is visible in the same
/// place a broken built-in ledger is, instead of looking like a company that
/// simply has no researcher.
pub fn faults() -> &'static [String] {
    &baseline().faults
}

/// Whether `key` (`<kind>:<id>`) names a global that exists.
///
/// The validator behind `[globals].disable`: an entry that matches nothing is a
/// manifest error, because the alternative is an opt-out an operator wrote,
/// believed, and never got.
pub fn has(key: &str) -> bool {
    let Some((kind, id)) = key.split_once(':') else {
        return false;
    };
    match kind {
        "agent" => agents().iter().any(|agent| agent.id == id),
        "workflow" => workflows().iter().any(|workflow| workflow.id == id),
        "skill" => skills().iter().any(|skill| skill.slug == id),
        "ledger" => ledgers().iter().any(|ledger| ledger.slug == id),
        "task" => tasks().iter().any(|task| task.id == id),
        _ => false,
    }
}

/// Whether `disable` (the manifest's `[globals].disable`) drops `<kind>:<id>`.
pub fn disabled(disable: &[String], kind: &str, id: &str) -> bool {
    let key = format!("{kind}:{id}");
    disable.iter().any(|entry| entry == &key)
}
