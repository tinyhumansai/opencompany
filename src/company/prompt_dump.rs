//! Rendering the system prompt an agent would be built with, without building
//! one.
//!
//! [`crate::company::prompt`] composes the pieces and
//! [`crate::harness::build::build_agent`] concatenates them, but that path
//! needs a live runtime — stores, an inference model, a materialized skill
//! catalogue — so the only way to see an agent's prompt used to be to run the
//! company and read a provider trace. That makes the most editable thing in a
//! bundle (`agents/prompts/*.md`, an inline `prompt`, a routed `context` entry)
//! the least inspectable, which is backwards: a brief is text an operator
//! writes and has no way to check.
//!
//! This module answers the same question from a manifest alone. It renders the
//! **persona body** — every section OpenCompany itself composes, in the order
//! the harness concatenates them — and names what it could not render and why,
//! rather than quietly returning a shorter prompt than the agent gets.
//!
//! # What is deliberately not here
//!
//! * **OpenHuman's own wrapper.** The body below is handed to
//!   `SystemPromptBuilder::for_subagent`, which prepends the runtime's safety
//!   preamble and appends its grounding and style suffix. Those bytes are the
//!   vendored runtime's, identical for every agent, and rendering them needs a
//!   `PromptContext` naming a workspace, a model and a live tool list. They are
//!   reported as a deferred section instead of guessed at.
//! * **Anything that only exists at runtime**: routed `context` bodies (a live
//!   workspace store), the skill catalogue (a materialized bundle directory),
//!   the MCP capability brief (a configured server registry). Each is listed in
//!   [`AgentPrompt::deferred`] with the reason, so a section missing from the
//!   dump is visibly missing rather than invisibly absent.
//!
//! # Why it lives beside `prompt.rs` rather than in the harness
//!
//! Same argument that module makes for itself: composition is ordinary text
//! manipulation, the harness is behind the `openhuman` feature, and a debugging
//! surface that only exists in a feature build is one nobody runs. The sections
//! the harness owns (the workspace, ledger, deliverable and delegation briefs)
//! are pulled in under `#[cfg(feature = "openhuman")]` from the *same*
//! functions the harness calls, so the dump cannot drift from them; a default
//! build renders the rest and says the briefs need `--features openhuman`.

use crate::company::{Agent, CompanyManifest, ContextEntry};

/// One rendered section of an agent's prompt.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Section {
    /// What this section is, for a human reading the dump.
    pub title: String,
    /// Where its bytes came from — a manifest field, a bundle file, a brief
    /// function. This is the line that turns "why is this in my prompt" into a
    /// file to open.
    pub origin: String,
    /// The bytes themselves, exactly as the harness would concatenate them.
    pub body: String,
}

/// A section the agent gets at runtime that a manifest alone cannot render.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Deferred {
    pub title: String,
    /// Why it is absent, in terms of what would have to exist to render it.
    pub reason: String,
}

/// One agent's prompt, as far as a manifest can tell.
#[derive(Debug, Clone, serde::Serialize)]
pub struct AgentPrompt {
    pub agent_id: String,
    pub role: String,
    pub tier: Option<String>,
    /// Whether this teammate came from the global baseline rather than the
    /// company's own roster (`globals/agents/*.toml`).
    pub global: bool,
    /// Whether this is the company's orchestrator — it changes the prompt, so
    /// it is reported rather than left to be inferred from `tier`.
    pub orchestrator: bool,
    /// This agent's effective tool grants: its own `tools` narrowed by the
    /// company `[tools].allow`. Several sections are wired off these, so a
    /// missing brief is usually a missing grant.
    pub grants: Vec<String>,
    pub sections: Vec<Section>,
    pub deferred: Vec<Deferred>,
}

impl AgentPrompt {
    /// The prompt body, exactly as the harness concatenates it: every rendered
    /// section, in order, with nothing between them.
    ///
    /// The sections are already self-delimiting — each brief begins with its
    /// own heading and leading newlines — so joining with a separator here
    /// would produce bytes the agent never sees.
    pub fn body(&self) -> String {
        self.sections
            .iter()
            .map(|section| section.body.as_str())
            .collect()
    }

    /// The annotated form: the same bytes, with a banner naming each section
    /// and where it came from, plus what could not be rendered.
    ///
    /// The bodies are fenced rather than inlined because a brief is Markdown
    /// itself — an unfenced dump of six briefs is one document whose headings
    /// all run together, and the reader cannot tell the report from the prompt.
    pub fn to_markdown(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!("# {} — {}\n\n", self.agent_id, self.role));
        out.push_str(&format!(
            "- tier: `{}`\n- orchestrator: {}\n- source: {}\n- grants: {}\n- rendered: {} chars across {} sections\n\n",
            self.tier.as_deref().unwrap_or("—"),
            if self.orchestrator { "yes" } else { "no" },
            if self.global {
                "global baseline (`globals/agents/`)"
            } else {
                "this company's roster"
            },
            if self.grants.is_empty() {
                "none".to_string()
            } else {
                self.grants
                    .iter()
                    .map(|grant| format!("`{grant}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            },
            self.body().chars().count(),
            self.sections.len(),
        ));
        for section in &self.sections {
            out.push_str(&format!("## {}\n\n_{}_\n\n", section.title, section.origin));
            out.push_str(&fence(&section.body));
            out.push('\n');
        }
        if !self.deferred.is_empty() {
            out.push_str("## Not rendered here\n\n");
            out.push_str(
                "These reach the agent at runtime. A manifest cannot produce them, so they are named rather than guessed at.\n\n",
            );
            for deferred in &self.deferred {
                out.push_str(&format!("- **{}** — {}\n", deferred.title, deferred.reason));
            }
            out.push('\n');
        }
        out
    }
}

/// Wraps `body` in a fence long enough to survive whatever fences it contains.
///
/// A brief is Markdown and several of them contain fenced examples, so a fixed
/// three-backtick fence would be closed by the prompt's own content and the
/// rest of the section would render as prose.
fn fence(body: &str) -> String {
    // A maximal run of n backticks splits into n-1 empty parts, so the run
    // length is the empty streak plus one. A lone backtick leaves no empty part
    // at all and is therefore invisible here — which costs nothing, because the
    // floor below is already three.
    let mut longest = 0usize;
    let mut streak = 0usize;
    for part in body.split('`') {
        if part.is_empty() {
            streak += 1;
            longest = longest.max(streak + 1);
        } else {
            streak = 0;
        }
    }
    let ticks = "`".repeat(longest.max(2) + 1);
    format!("{ticks}text\n{}\n{ticks}\n", body.trim_matches('\n'))
}

/// Render every agent in `manifest`, roster order, globals included.
///
/// Globals are included on purpose: a global teammate is a real agent in this
/// company with a prompt of its own, and the most common question this surface
/// answers ("why did the researcher say that") is about one of them.
pub fn dump(manifest: &CompanyManifest) -> Vec<AgentPrompt> {
    let orchestrator = crate::company::orchestrator_id(&manifest.agents).map(str::to_string);
    manifest
        .agents
        .iter()
        .map(|agent| {
            dump_agent(
                manifest,
                agent,
                orchestrator.as_deref() == Some(agent.id.as_str()),
            )
        })
        .collect()
}

fn dump_agent(manifest: &CompanyManifest, agent: &Agent, orchestrator: bool) -> AgentPrompt {
    let grants = crate::runtime::builder::agent_effective_grants(
        &manifest.tools.allow,
        agent.tools.as_deref(),
    );
    let mut sections = Vec::new();
    let mut deferred = Vec::new();

    sections.push(Section {
        title: "Persona".to_string(),
        origin: format!(
            "generated framing + `role` / `description` / `prompt` in `agents/{}.toml`",
            agent.id
        ),
        body: crate::company::prompt::persona_prompt(
            &manifest.company.name,
            agent,
            agent.prompt.as_deref(),
        ),
    });

    let bundle = crate::company::prompt::bundle_section(agent);
    if bundle.is_empty() {
        deferred.push(Deferred {
            title: "Your brief".to_string(),
            reason: format!(
                "`agents/{}.toml` names no `prompt_files`, so this agent has no checked-in brief",
                agent.id
            ),
        });
    } else {
        sections.push(Section {
            title: "Your brief".to_string(),
            origin: format!(
                "`prompt_files` = {:?}, read from `agents/`",
                agent.prompt_files
            ),
            body: bundle,
        });
    }

    harness_sections(&grants, agent, orchestrator, &mut sections, &mut deferred);

    deferred.push(Deferred {
        title: "Working documents".to_string(),
        reason: match agent.context.as_deref() {
            // Absent and empty are different declarations, and conflating them
            // would mislead exactly the operator this surface is for: no
            // `context` key means the routing defaults apply, while an empty
            // list is a deliberate "route me nothing".
            None => "`context` is unset, so this agent takes the default routing set \
                     (`company::context_routing`) resolved against a live workspace store"
                .to_string(),
            Some([]) => "`context = []` — this agent deliberately routes no documents".to_string(),
            Some(entries) => format!(
                "routed from a live workspace store at build time: {}",
                context_paths(entries).join(", ")
            ),
        },
    });
    deferred.push(Deferred {
        title: "OpenHuman preamble, grounding and style suffix".to_string(),
        reason: "the vendored runtime wraps this body in `SystemPromptBuilder::for_subagent`; \
                 rendering it needs a live `PromptContext` (workspace, model, tool list)"
            .to_string(),
    });

    AgentPrompt {
        agent_id: agent.id.clone(),
        role: agent.role.clone(),
        tier: agent.tier.clone(),
        global: agent.global,
        orchestrator,
        grants,
        sections,
        deferred,
    }
}

fn context_paths(context: &[ContextEntry]) -> Vec<String> {
    context
        .iter()
        .map(|entry| format!("`{}`", entry.path()))
        .collect()
}

/// The sections the harness owns, rendered from the harness's own brief
/// functions so the dump cannot drift from what an agent is actually built
/// with.
#[cfg(feature = "openhuman")]
fn harness_sections(
    grants: &[String],
    agent: &Agent,
    orchestrator: bool,
    sections: &mut Vec<Section>,
    deferred: &mut Vec<Deferred>,
) {
    if crate::harness::build::grants_cover(grants, "workspace") {
        let writes = crate::company::grants_workspace_write_explicit(grants);
        sections.push(Section {
            title: "Workspace".to_string(),
            origin: format!(
                "`harness::workspace_tools::workspace_brief` (writes: {})",
                if writes { "granted" } else { "read-only" }
            ),
            body: crate::harness::workspace_tools::workspace_brief(writes),
        });
    } else {
        deferred.push(Deferred {
            title: "Workspace".to_string(),
            reason:
                "this agent's grants do not cover `workspace`, so no workspace tools and no brief"
                    .to_string(),
        });
    }

    // The built-in registry only. A company-declared ledger is declared while
    // the company runs, so a manifest cannot know about one — the brief the
    // agent sees will name more than this does, and saying so is the point.
    let registry = crate::ledger::Registry::build([]);
    sections.push(Section {
        title: "Ledgers".to_string(),
        origin: "`harness::ledger_tools::ledger_brief` over the built-in registry — a ledger this company declared at runtime is not in it".to_string(),
        body: crate::harness::ledger_tools::ledger_brief(&registry),
    });

    // Mirrors `build_agent`'s ordering — the sandbox is described before the
    // deliverable brief that presumes it. Each of the three namespaces is read
    // off the same predicates the builder gates its tool vectors on, so a dump
    // cannot claim a clause the real prompt withholds. `shell` here is the
    // GRANT, where the builder additionally requires the audit logger to have
    // initialized; a manifest cannot know that, so the origin line says so
    // rather than letting the dump imply a guarantee it does not have.
    let sandbox_files = crate::company::grants_files_or_docs(grants);
    let sandbox_shell = crate::harness::build::grants_cover(grants, "shell");
    let sandbox_code = crate::harness::build::grants_cover(grants, "code");
    let sandbox =
        crate::harness::toolbelt::sandbox_brief(sandbox_files, sandbox_shell, sandbox_code);
    if sandbox.is_empty() {
        deferred.push(Deferred {
            title: "Your sandbox".to_string(),
            reason: "this agent's grants cover none of `files`/`docs`, `shell` or `code`, so it is offered no tool that reaches its working directory".to_string(),
        });
    } else {
        sections.push(Section {
            title: "Your sandbox".to_string(),
            origin: "`harness::toolbelt::sandbox_brief` — one clause per granted namespace; the `shell` clause additionally needs the per-agent audit logger to initialize at build time".to_string(),
            body: sandbox,
        });
    }

    if crate::company::grants_files_or_docs(grants) {
        sections.push(Section {
            title: "Deliverables".to_string(),
            origin: "`harness::publish::publish_brief` — wired on the `files`/`docs` grant, and only when an artifact store is configured".to_string(),
            body: crate::harness::publish::publish_brief(),
        });
    } else {
        deferred.push(Deferred {
            title: "Deliverables".to_string(),
            reason: "this agent's grants cover neither `files` nor `docs`, so it gets no `publish_artifact` tool".to_string(),
        });
    }

    deferred.push(Deferred {
        title: "Skill catalogue".to_string(),
        reason: "materialized from the skill registry and per-agent deltas into the agent's sandbox at build time".to_string(),
    });

    if orchestrator {
        sections.push(Section {
            title: "Orchestrator".to_string(),
            origin: "`harness::built_in::orchestrator::orchestrator_brief`".to_string(),
            body: crate::harness::built_in::orchestrator::orchestrator_brief(),
        });
    } else if !agent.delegates_to.is_empty() {
        sections.push(Section {
            title: "Delegation".to_string(),
            origin: format!(
                "`harness::built_in::orchestrator::member_delegation_brief`, narrowed to {:?}",
                agent.delegates_to
            ),
            body: crate::harness::built_in::orchestrator::member_delegation_brief(
                &agent.delegates_to,
            ),
        });
    }

    deferred.push(Deferred {
        title: "MCP capability brief".to_string(),
        reason: "appended only when this agent is granted an enabled MCP server, which needs a configured registry".to_string(),
    });

    // Issue #1759: the connected-integration grounding + Composio-routing brief.
    // Appended by `build_agent` only when the Composio tools are actually wired
    // (an explicit `composio` grant AND a credential resolved for this company),
    // and its text names this company's connected toolkits — neither of which a
    // manifest alone can know, so it is reported deferred rather than guessed at.
    //
    // PR #1780 review: the composition call site in `build_agent` is compiled
    // only under `#[cfg(feature = "composio")]` (`harness/built_in/build.rs`),
    // so a binary built without that feature — the standard
    // `scripts/dump-prompt.sh` invocation enables only `openhuman` — can never
    // include this brief, no matter what the grant or credential state is at
    // runtime. Saying "may appear once the grant and credential resolve" in
    // that build misdescribes a compile-time absence as a runtime-deferred
    // one. Mirror the `#[cfg(not(feature = "openhuman"))]` split below: name
    // the missing feature instead of implying the section is reachable.
    #[cfg(feature = "composio")]
    deferred.push(Deferred {
        title: "Connected integrations brief".to_string(),
        reason: "appended only when this agent explicitly grants `composio` and a Composio credential resolves for the company; its toolkit list needs the live connection state".to_string(),
    });
    #[cfg(not(feature = "composio"))]
    deferred.push(Deferred {
        title: "Connected integrations brief".to_string(),
        reason: "this binary was built without `--features composio`, so the harness that composes this brief is not linked; rebuild with `--features openhuman,composio` to see it".to_string(),
    });
}

#[cfg(not(feature = "openhuman"))]
fn harness_sections(
    _grants: &[String],
    _agent: &Agent,
    _orchestrator: bool,
    _sections: &mut Vec<Section>,
    deferred: &mut Vec<Deferred>,
) {
    deferred.push(Deferred {
        title: "Tool briefs (workspace, ledgers, deliverables, delegation)".to_string(),
        reason: "this binary was built without `--features openhuman`, so the harness that owns those briefs is not linked".to_string(),
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(body: &str) -> CompanyManifest {
        toml::from_str(body).expect("test manifest parses")
    }

    #[test]
    fn the_persona_section_is_always_present_and_names_the_company() {
        let manifest = manifest(
            "[company]\nname = \"Acme\"\n\n[[agent]]\nid = \"pm\"\nrole = \"Product Manager\"\n",
        );
        let dumped = dump(&manifest);
        assert_eq!(dumped.len(), 1);
        assert_eq!(dumped[0].sections[0].title, "Persona");
        assert!(
            dumped[0].sections[0]
                .body
                .contains("Product Manager at Acme"),
            "{}",
            dumped[0].sections[0].body
        );
    }

    /// The first agent is the orchestrator when nobody is tagged, and the dump
    /// must report that rather than leaving it to be inferred from an absent
    /// `tier` — the orchestrator brief is a real section of its prompt.
    #[test]
    fn the_untagged_first_agent_is_reported_as_the_orchestrator() {
        let manifest = manifest(
            "[company]\nname = \"Acme\"\n\n[[agent]]\nid = \"first\"\nrole = \"First\"\n\n[[agent]]\nid = \"second\"\nrole = \"Second\"\n",
        );
        let dumped = dump(&manifest);
        assert!(dumped[0].orchestrator);
        assert!(!dumped[1].orchestrator);
    }

    /// The sandbox section is the one this surface most needs to get right: an
    /// agent that is never told it holds `file_write` records a task about
    /// writing instead of writing, and an operator reading a dump that omits
    /// the section has no way to see why. A default belt (`[tools].allow`
    /// defaults to `*`) must therefore produce it, naming all three clauses.
    ///
    /// `harness_sections` — the only place that ever adds or defers a "Your
    /// sandbox" section — is itself `#[cfg(feature = "openhuman")]`; a
    /// default build folds it into the single "Tool briefs (...)" deferred
    /// line instead (see the `#[cfg(not(feature = "openhuman"))]` branch
    /// above). This test needs the same feature gate its subject does.
    #[test]
    #[cfg(feature = "openhuman")]
    fn a_default_belt_is_told_about_its_sandbox_and_its_shell() {
        let manifest = manifest(
            "[company]\nname = \"Acme\"\n\n[[agent]]\nid = \"pm\"\nrole = \"Product Manager\"\n",
        );
        let dumped = dump(&manifest);
        let section = dumped[0]
            .sections
            .iter()
            .find(|s| s.title == "Your sandbox")
            .expect("a `*` belt covers files, shell and code");
        for tool in ["file_write", "shell", "apply_patch"] {
            assert!(section.body.contains(tool), "{}", section.body);
        }
    }

    /// The inverse, and the reason the section is gated at all: a belt that
    /// reaches none of the three namespaces must report the absence rather than
    /// describe tools this agent cannot call.
    ///
    /// Same feature gate as above — without `openhuman`, this manifest's
    /// absence gets folded into the generic "Tool briefs (...)" deferred
    /// line rather than a "Your sandbox" one.
    #[test]
    #[cfg(feature = "openhuman")]
    fn a_belt_with_no_sandbox_namespace_defers_the_section() {
        let manifest = manifest(
            "[company]\nname = \"Acme\"\n\n[tools]\nallow = [\"workspace\"]\n\n[[agent]]\nid = \"pm\"\nrole = \"Product Manager\"\ntools = [\"workspace\"]\n",
        );
        let dumped = dump(&manifest);
        assert!(
            !dumped[0].sections.iter().any(|s| s.title == "Your sandbox"),
            "{:?}",
            dumped[0]
                .sections
                .iter()
                .map(|s| &s.title)
                .collect::<Vec<_>>()
        );
        assert!(
            dumped[0]
                .deferred
                .iter()
                .any(|entry| entry.title == "Your sandbox"),
            "{:?}",
            dumped[0].deferred
        );
    }

    /// An agent with no `prompt_files` has no brief, and that is reported as a
    /// deferred line rather than silently producing a shorter prompt — the
    /// whole point of the surface is to explain what an agent is missing.
    #[test]
    fn an_agent_without_prompt_files_reports_the_absent_brief() {
        let manifest = manifest(
            "[company]\nname = \"Acme\"\n\n[[agent]]\nid = \"pm\"\nrole = \"Product Manager\"\n",
        );
        let dumped = dump(&manifest);
        assert!(
            dumped[0]
                .deferred
                .iter()
                .any(|entry| entry.title == "Your brief"),
            "{:?}",
            dumped[0].deferred
        );
    }

    /// `body()` must be the concatenation the harness performs, with nothing
    /// inserted: a separator here would be bytes the agent never sees, and the
    /// dump would stop being usable as a diff against a provider trace.
    #[test]
    fn the_body_is_the_sections_concatenated_with_nothing_between_them() {
        let manifest = manifest(
            "[company]\nname = \"Acme\"\n\n[[agent]]\nid = \"pm\"\nrole = \"PM\"\nprompt = \"Be brief.\"\n",
        );
        let dumped = dump(&manifest);
        let expected: String = dumped[0]
            .sections
            .iter()
            .map(|section| section.body.clone())
            .collect();
        assert_eq!(dumped[0].body(), expected);
        assert!(dumped[0].body().contains("Be brief."));
    }

    /// A brief containing a fenced block must not close the fence the report
    /// wraps it in, or half the prompt renders as prose in the dump.
    #[test]
    fn a_body_containing_a_fence_is_wrapped_in_a_longer_one() {
        let fenced = fence("before\n```rust\nlet x = 1;\n```\nafter");
        assert!(fenced.starts_with("````text\n"), "{fenced}");
        assert!(fenced.trim_end().ends_with("````"), "{fenced}");
    }

    /// PR #1780 review: `build_agent`'s call site for the connected-integrations
    /// brief is compiled only under `#[cfg(feature = "composio")]`
    /// (`harness/built_in/build.rs`), so in a binary built without that
    /// feature — the standard `scripts/dump-prompt.sh` invocation enables only
    /// `openhuman` — the brief can never be appended, no matter what the grant
    /// or credential state is at runtime. The deferred reason must say the
    /// feature is missing, not describe compile-time absence as something that
    /// "may appear once the grant and credential resolve".
    ///
    /// Same feature gate as the sandbox tests above: `harness_sections` under
    /// `#[cfg(feature = "openhuman")]` is the only place that pushes this
    /// entry, and this test needs `composio` compiled *out* to reach the
    /// branch it is checking.
    #[test]
    #[cfg(all(feature = "openhuman", not(feature = "composio")))]
    fn without_composio_the_connected_integrations_brief_names_the_missing_feature() {
        let manifest = manifest(
            "[company]\nname = \"Acme\"\n\n[[agent]]\nid = \"pm\"\nrole = \"Product Manager\"\n",
        );
        let dumped = dump(&manifest);
        let entry = dumped[0]
            .deferred
            .iter()
            .find(|d| d.title == "Connected integrations brief")
            .expect("always deferred outside a live build");
        assert!(
            entry.reason.contains("--features composio"),
            "a binary without `composio` can never compile in the call site that appends \
             this brief, so the reason must name the missing feature: {}",
            entry.reason
        );
        assert!(
            !entry.reason.contains("credential resolves"),
            "must not describe this as runtime-deferred when a `composio`-less binary can \
             never include it regardless of grant or credential state: {}",
            entry.reason
        );
    }

    /// The inverse of the test above: once `composio` IS compiled in, the
    /// section really is runtime-deferred (an explicit grant plus a resolved
    /// credential decide it), so the reason must keep describing that instead
    /// of claiming the feature is missing.
    #[test]
    #[cfg(feature = "composio")]
    fn with_composio_the_connected_integrations_brief_stays_runtime_deferred() {
        let manifest = manifest(
            "[company]\nname = \"Acme\"\n\n[[agent]]\nid = \"pm\"\nrole = \"Product Manager\"\n",
        );
        let dumped = dump(&manifest);
        let entry = dumped[0]
            .deferred
            .iter()
            .find(|d| d.title == "Connected integrations brief")
            .expect("always deferred outside a live build");
        assert!(
            entry.reason.contains("credential resolves"),
            "a `composio`-enabled binary can still include this brief once the grant and \
             credential resolve, so the reason must keep saying so: {}",
            entry.reason
        );
        assert!(
            !entry.reason.contains("--features composio"),
            "must not claim the feature is missing when it is compiled in: {}",
            entry.reason
        );
    }
}
