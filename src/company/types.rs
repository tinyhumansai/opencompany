//! Serde-facing types for the [Company Manifest](../../docs/spec/runtime/manifest.md).
//!
//! Enum-like fields (`brain.mode`, `policy.mode`, `tools.provider`, agent
//! `tier`, channel names) are deserialized as plain strings and validated in
//! [`super::manifest`] so that errors read in prosumer language instead of
//! serde traces.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Cognition tiers a company may hint per agent. The client only names a tier;
/// the TinyHumans backend maps tier → model SKU.
pub const TIERS: &[&str] = &[
    "orchestrator",
    "reasoning",
    "frontend",
    "compress",
    "subconscious",
];

/// Brain implementations selectable in `[brain].mode`.
pub const BRAIN_MODES: &[&str] = &["hosted", "sidecar"];

/// Sign-in modes selectable in `[users].mode`. See
/// [`AuthMode`](crate::app::config::AuthMode) for what each one means.
pub const AUTH_MODES: &[&str] = &["email", "wallet", "none"];

/// Inference providers selectable in `[inference].provider` (issue #56 — BYOK).
///
/// * `openrouter` — OpenRouter's OpenAI-compatible aggregator, and the default.
///   With **no** key it resolves to the platform endpoint and the subscription
///   pays; with a tenant `sk-or-…` it goes direct to OpenRouter on the tenant's
///   own account. Either way it carries OpenRouter's `HTTP-Referer` / `X-Title`
///   attribution headers.
/// * `openai_compatible` — any OpenAI-compatible endpoint the tenant runs
///   (needs a `base_url`, usually a key).
/// * `ollama` — a local Ollama server's OpenAI-compatible surface (needs a
///   `base_url`; no key).
///
/// `managed` was removed: OpenCompany no longer exposes its own model SKUs, so
/// there is nothing for a distinct managed kind to name. A manifest or stored
/// runtime blob still saying `managed` aliases to `openrouter` — see
/// [`inference::LEGACY_MANAGED`](crate::company::inference::LEGACY_MANAGED).
pub const INFERENCE_PROVIDERS: &[&str] = &["openrouter", "openai_compatible", "ollama"];

/// Harness kinds selectable in `[[harness]].kind`.
///
/// * `built_in` — the embedded OpenHuman loop, in this process, against the
///   inference provider the harness itself declares.
/// * `acp` — an external agent driven over the Agent Client Protocol. Runs on
///   whatever credential *it* holds, so it needs no `[harness.inference]`.
pub const HARNESS_KINDS: &[&str] = &["built_in", "acp"];

/// The default harness kind, used for the implicit harness a company with no
/// `[[harness]]` block gets.
pub const DEFAULT_HARNESS_KIND: &str = "built_in";

/// The id given to the implicit harness synthesized for a company that declares
/// no `[[harness]]` block.
pub const IMPLICIT_HARNESS_ID: &str = "default";

/// Transports selectable in `[harness.acp].transport`.
///
/// A remote runner is a *transport*, not a third harness kind: `RunnerDispatch`
/// already implements the same `AcpAgent` port the local subprocess does, so the
/// only thing that differs is how bytes reach the agent.
pub const ACP_TRANSPORTS: &[&str] = &["local", "runner"];

/// ACP agents selectable in `[harness.acp].agent`, for `transport = "local"`.
///
/// Kept in step with the desktop's own `ACP_HARNESSES` catalogue, which encodes
/// how to put each one into ACP mode — guessing those arguments wrong spawns a
/// process that hangs waiting for interactive input.
pub const ACP_AGENTS: &[&str] = &["claude", "codex"];

/// The abstract cognition tiers the tenant `[inference].models` table maps to
/// concrete provider model ids. These are the workload names the harness
/// addresses; an unmapped tier passes through to the provider verbatim.
pub const INFERENCE_TIERS: &[&str] = &["chat-v1", "reasoning-v1", "agentic-v1", "vision-v1"];

/// Tool providers selectable in `[tools].provider`.
pub const TOOL_PROVIDERS: &[&str] = &["openhuman", "builtin"];

/// Approval policy modes selectable in `[policy].mode`, in increasing order of
/// autonomy.
///
/// `readonly` / `supervised` / `full` take their names from OpenHuman's own
/// security tiers; `auto` (issue #560) is opencompany's, and sits between
/// "ask before every change" and "ask before almost nothing".
///
/// This list is the manifest validator's, so it is the gate that decides whether
/// a mode is *reachable* at all — `PolicyMode::parse` never sees a string this
/// rejects. The two are kept in step by `every_policy_mode_parses_to_its_own_
/// variant` in `harness::policy`.
pub const POLICY_MODES: &[&str] = &["readonly", "supervised", "auto", "full"];

/// The tier a **newly provisioned** company is given when its manifest does not
/// name one (issue #605).
///
/// Deliberately *not* the same knob as [`default_policy_mode`]. That one answers
/// for every manifest ever parsed, including the re-parse of a company that has
/// been running for months, so moving it changes companies rather than creating
/// them. This one is applied once, at provisioning, and written into the stored
/// manifest as an ordinary explicit `mode` — after which nothing distinguishes
/// the company from one whose author typed the line themselves.
///
/// See `docs/spec/company-brain/approvals.md` for why `auto` is the defensible
/// default for a new company and why that argument does not extend to
/// retroactively re-tiering existing ones.
pub const PROVISIONED_POLICY_MODE: &str = "auto";

/// Channels the runtime knows how to enable under `[channels.*]`.
pub const KNOWN_CHANNELS: &[&str] = &["operator", "email", "slack", "sms", "web"];

/// Effect kinds gated for approval by default — **empty on purpose** (issue
/// #684).
///
/// This shipped as `["payment.send", "filing.submit", "external.publish"]`, and
/// the promise it read as was not one the runtime kept. `always_approve` wins
/// over every tier including `full`, so a company shipping the default believed
/// payments, filings and publishing were gated. On the **harness** path —
/// the one every company using the openhuman toolbelt runs — the list is
/// matched against the tool name, and none of those three names a tool, so
/// nothing was gated at all.
///
/// The list is now matched by one shared rule on both paths
/// ([`always_approve::matches`](crate::policy::always_approve::matches)), and it
/// ships empty rather than being repaired, for three reasons:
///
/// * **Two of the three name capabilities this product does not have.** There
///   is no payment tool and no `Sign`-group tool in the declaration table, and
///   nothing outside test code emits either kind. A default cannot gate a
///   capability that does not exist.
/// * **The third must not be defaulted.** The real name behind
///   `external.publish` is `publish_artifact`, and issue #658 ruled that `full`
///   publishes unattended — an operator who wants otherwise writes
///   `always_approve = ["publish_artifact"]`. Defaulting it would overturn that
///   ruling silently.
/// * **It costs no protection.** The default mode is `supervised`, and
///   `ManifestApprovalGate::evaluate_supervised` already parks every `Spend`,
///   `Sign` and `Publish` effect on its own. The three entries added nothing to
///   the default configuration; they only ever mattered under `auto` and
///   `full`, tiers an operator opts into for unattended operation.
///
/// An operator who wants a specific gate still writes one. Operator-authored
/// effect kinds remain open-ended because a hosted brain may emit a kind this
/// repository has never seen; see [`crate::policy::always_approve`] for why a
/// registry-based validator would reject working custom fences.
pub const DEFAULT_ALWAYS_APPROVE: &[&str] = &[];

/// Priorities a company may assign to a prioritized `[[connection]]`.
pub const CONNECTION_PRIORITIES: &[&str] = &["low", "medium", "high"];

/// The exec tool-grant namespaces a capability [`Plan`] can budget (issue #108).
///
/// This is the canonical, always-compiled source of truth for "which tool
/// families are gateable"; the harness re-exports it as
/// [`GATEABLE_NAMESPACES`](crate::harness::toolbelt::GATEABLE_NAMESPACES) and
/// maps individual tools onto these namespaces. A `[plan].token_budgets` key
/// outside this set is a manifest error. Lives here (not the feature-gated
/// harness) so manifest validation can see it in the default build.
pub const GATEABLE_NAMESPACES: [&str; 7] = [
    "shell", "code", "web", "subagent", "media", "composio", "search",
];

/// Whether a tool-grant list **explicitly** grants the real-money `media`
/// namespace (issue #109).
///
/// Unlike the ordinary namespace match, the catch-all `*` does **not** grant
/// `media`: a capability that spends real money on image/video generation must
/// be opted into by name, never ridden in on a wildcard. Matches the bare
/// `media` grant or any `media.*` sub-grant. Lives here (always compiled) so
/// both the feature-gated harness wiring (`build::build_agent`) and the
/// always-compiled console capability route key off one source of truth.
pub fn grants_media_explicit(grants: &[String]) -> bool {
    grants
        .iter()
        .any(|grant| grant == "media" || grant.starts_with("media."))
}

/// Whether a tool-grant list **explicitly** grants the per-tenant `composio`
/// namespace (issue #110).
///
/// Like [`grants_media_explicit`], the catch-all `*` does **not** grant
/// `composio`: the Composio tools reach third-party accounts (Gmail / Slack /
/// GitHub) over a tenant OAuth token and move real side effects (send email,
/// open PRs), so they must be opted into by name, never ridden in on a
/// wildcard. Matches the bare `composio` grant or any `composio.*` sub-grant.
/// Lives here (always compiled) so both the feature-gated harness wiring
/// (`build::build_agent`) and the always-compiled console capability route key
/// off one source of truth.
pub fn grants_composio_explicit(grants: &[String]) -> bool {
    grants
        .iter()
        .any(|grant| grant == "composio" || grant.starts_with("composio."))
}

/// Whether a tool-grant list **explicitly** grants the `chargebee` billing
/// namespace (issue #788).
///
/// Like [`grants_composio_explicit`], the catch-all `*` does **not** grant it:
/// these tools send invoices to real customers of a real business, so they are
/// opted into by name rather than ridden in on a wildcard a company set for its
/// file and shell tools. Lives here (always compiled) so the feature-gated
/// harness wiring and the always-compiled console capability route key off one
/// source of truth.
pub fn grants_chargebee_explicit(grants: &[String]) -> bool {
    grants
        .iter()
        .any(|grant| grant == "chargebee" || grant.starts_with("chargebee."))
}

/// Whether a tool-grant list **explicitly** grants the `hosting` namespace.
///
/// Like its siblings, the catch-all `*` does **not** confer it. These tools
/// publish a company's files to the public internet under its own name and can
/// provision a managed database it is billed for, so they are opted into by
/// name rather than ridden in on a wildcard a company set for its file and
/// shell tools.
pub fn grants_hosting_explicit(grants: &[String]) -> bool {
    grants
        .iter()
        .any(|grant| grant == "hosting" || grant.starts_with("hosting."))
}

/// The grant list a teammate created with **no stated `tools`** should receive.
///
/// An omitted `tools` line means "the company's standard grant", and the
/// standard grant is the whole of `[tools].allow`. That is the right default
/// for the belt a company runs on — issue #1674 made it wider on purpose,
/// because a teammate minted from three sentences in a wizard reporting its
/// own tools as "not enabled" is the worse failure. It is the wrong default
/// for a namespace `*` deliberately refuses to confer: a company that added
/// `chargebee` by name so that ONE teammate could invoice should not hand
/// billing to the next teammate an operator types into the console.
///
/// So this withholds exactly the **BYO real-money** namespaces — the ones a
/// company only ever holds because somebody named them, and that reach a real
/// business's customers, wallet, or public identity. `media`, `composio` and
/// `search` are deliberately NOT withheld: they ship in the default belt, so
/// withholding them would re-create the #1674 complaint for every new
/// teammate.
///
/// Returns **empty** when nothing is withheld, preserving the "empty means the
/// standard company grant" contract for the overwhelming majority of companies
/// that grant none of these. A non-empty return is the allow-list minus the
/// withheld namespaces, materialised so the stored teammate carries its own
/// narrowed line rather than inheriting a ceiling that later widens.
pub fn creation_default_grants(allow: &[String]) -> CreationGrant {
    let withheld = |grant: &String| {
        let one = std::slice::from_ref(grant);
        grants_chargebee_explicit(one)
            || grants_paypal_explicit(one)
            || grants_hosting_explicit(one)
    };
    if !allow.iter().any(withheld) {
        return CreationGrant::Standard;
    }
    let kept: Vec<String> = allow.iter().filter(|g| !withheld(g)).cloned().collect();
    if kept.is_empty() {
        return CreationGrant::NothingLeft;
    }
    CreationGrant::Narrowed(kept)
}

/// What [`creation_default_grants`] decided for a teammate created with no
/// stated `tools`.
///
/// Three cases rather than a `Vec`, because a `Vec` cannot express the third
/// one: an empty list is already spoken for — it means "the standard company
/// grant" — so returning the filtered-to-nothing result as `vec![]` would hand
/// back the exact capability the filter just removed. The orchestrator's
/// `add_agent` refuses on the same reasoning when narrowing a requested scope
/// yields nothing, and this mirrors it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CreationGrant {
    /// Nothing was withheld. Store an empty line, which keeps tracking
    /// `[tools].allow` the way an unstated grant always has.
    Standard,
    /// Store this narrowed line: the allow-list minus the withheld namespaces.
    Narrowed(Vec<String>),
    /// The company's whole belt is withheld namespaces (`allow = ["chargebee"]`
    /// and nothing else). There is no safe line to store — empty would read
    /// back as inheritance — so the caller refuses and asks for an explicit
    /// grant instead.
    NothingLeft,
}

/// Whether a tool-grant list **explicitly** grants the `paypal` namespace
/// (issue #789).
///
/// Like its siblings, the catch-all `*` does **not** grant it. These tools read
/// a real business's wallet, so they are opted into by name.
pub fn grants_paypal_explicit(grants: &[String]) -> bool {
    grants
        .iter()
        .any(|grant| grant == "paypal" || grant.starts_with("paypal."))
}

/// Whether a tool-grant list **explicitly** grants the metered `search`
/// namespace (issue #238).
///
/// Like [`grants_media_explicit`] and [`grants_composio_explicit`], the
/// catch-all `*` does **not** grant it: every `web_search` call is a priced
/// request on the managed platform, so it must be opted into by name rather
/// than ridden in on a wildcard a company set for its file and shell tools.
/// Matches the bare `search` grant or any `search.*` sub-grant. Lives here
/// (always compiled) so both the feature-gated harness wiring
/// (`build::build_agent`) and the always-compiled console capability route key
/// off one source of truth.
pub fn grants_search_explicit(grants: &[String]) -> bool {
    grants
        .iter()
        .any(|grant| grant == "search" || grant.starts_with("search."))
}

/// The [`GATEABLE_NAMESPACES`] a built-in tool can serve directly — the shared
/// native-capability vocabulary both native-first routing levers key off.
///
/// It is `GATEABLE_NAMESPACES` minus `composio` (the third-party connection
/// path, never a built-in tool) and minus `web` (the raw-HTTP family the
/// Composio deflection guardrail governs). A future native tool flows into both
/// levers by its [`namespace_of`](crate::harness::toolbelt::namespace_of) arm
/// landing in this set; nothing here is a literal capability name.
pub fn native_capability_namespaces() -> Vec<&'static str> {
    GATEABLE_NAMESPACES
        .iter()
        .copied()
        .filter(|ns| *ns != "composio" && *ns != "web")
        .collect()
}

/// Whether a grant list confers the native namespace `ns`, mirroring the
/// harness wiring gate: the real-money `search`/`media` families through their
/// explicit grant helpers (the catch-all `*` never confers them), and every
/// other namespace through the ordinary namespace rule a bare `*` satisfies.
pub fn grants_confer_native(grants: &[String], ns: &str) -> bool {
    use crate::runtime::tools::{NAMESPACE_SEPARATORS, extends_on_boundary};
    match ns {
        "search" => grants_search_explicit(grants),
        "media" => grants_media_explicit(grants),
        _ => grants
            .iter()
            .any(|grant| grant == "*" || extends_on_boundary(grant, ns, NAMESPACE_SEPARATORS)),
    }
}

/// Whether a tool-grant list confers the **publishing** capability (issue #244)
/// — the `files`/`docs` namespace on which both an agent's file tools and
/// `publish_artifact` ride.
///
/// # This is deliberately NOT a member of the `_explicit` family above
///
/// Read that first, because the naming invites exactly the wrong edit. Every
/// `grants_*_explicit` sibling guards a surface that spends real money or
/// reaches a third party, and for those the catch-all `*` confers **nothing**:
/// a decision that size is made by name. Publishing is the opposite case. It
/// spends nothing and reaches nothing outside the company's own board, so it
/// rides the ordinary namespace rule and **a bare `*` DOES confer it** — which
/// is what `build_agent`'s own gate has always done.
///
/// So do not "harmonise" this into the `_explicit` shape. The overwhelming
/// majority of shipped manifests grant `*`; renaming this predicate into that
/// family would silently revoke publishing for all of them, and the only
/// symptom would be agents that can write files and quietly cannot deliver
/// them.
///
/// # One derivation, two callers
///
/// [`build_agent`](crate::harness::build::build_agent)'s `wants_files` gate —
/// which decides whether `publish_artifact` (and the file belt) is wired at all
/// — and the always-compiled console capability route both call this. That is
/// the point: a second copy of the rule is how the panel comes to report a
/// capability the toolbelt does not actually wire (issue #886), and a
/// hand-rolled `starts_with` here would additionally re-fork the boundary rule
/// issue #461 de-forked.
///
/// Lives in this module rather than in `harness::build` because `harness` is
/// behind the `openhuman` feature and the console route is not.
///
/// A caller of
/// [`extends_on_boundary`](crate::runtime::tools::extends_on_boundary) over the
/// grant separator set, so `docs`, `docs.read`, `files.write` and `*` all
/// confer it while `documentation` — which a naive prefix test would accept —
/// does not.
pub fn grants_files_or_docs(grants: &[String]) -> bool {
    use crate::runtime::tools::{NAMESPACE_SEPARATORS, extends_on_boundary};
    grants.iter().any(|grant| {
        grant == "*"
            || extends_on_boundary(grant, "files", NAMESPACE_SEPARATORS)
            || extends_on_boundary(grant, "docs", NAMESPACE_SEPARATORS)
    })
}

/// Whether a tool-grant list **explicitly** grants writes to the company
/// workspace (issue #237).
///
/// Reading the workspace follows the ordinary namespace rule, so a catch-all
/// `*` confers it. **Writing does not**: a workspace write overwrites
/// operator-owned guidance that every other agent then treats as the company's
/// source of truth, so — like [`grants_media_explicit`] and
/// [`grants_composio_explicit`] — it must be opted into by name.
///
/// It is deliberately *tighter* than those two, which match any `<ns>.` prefix.
/// Here only the bare `workspace` grant and the exact `workspace.write`
/// sub-grant confer writes, so `workspace.read` is a genuinely read-only grant
/// rather than a footgun that silently hands over write access to the same
/// tree. Lives here (always compiled) so the feature-gated harness wiring
/// (`build::build_agent`) and always-compiled manifest tooling share one source
/// of truth.
pub fn grants_workspace_write_explicit(grants: &[String]) -> bool {
    grants
        .iter()
        .any(|grant| grant == "workspace" || grant == "workspace.write")
}

/// Epistemic classes a roster teammate may declare, gating which routed
/// documents it may be told (`docs/spec/runtime/orchestration/context-routing.md`).
///
/// The routing spec's three exclusions quantify over "roles that weigh
/// evidence", "roles that judge" and "roles acting on a directive", and it is
/// explicit that the classification MUST be an explicit declaration and **never**
/// a match on `role`: `role` is prose an operator writes for humans, so matching
/// on it would make a company that renames "Critic" to "Reviewer" silently lose
/// an exclusion, and a control a rename can switch off is not a control.
///
/// * `evidence` — weighs evidence, so it is never routed the assertion board.
/// * `judge` — scores a deliverable, so it is never routed the scratch.
/// * `directive` — acts on an operator instruction, so it is never routed the
///   claim ledger.
///
/// Declaring none (the default) is *unclassified*, which imposes no exclusion —
/// the correct default, because an ordinary teammate is not judging anything.
pub const PROMPT_CLASSES: [&str; 3] = ["evidence", "judge", "directive"];

/// Ceiling, in Unicode codepoints, on the prompt text one agent's
/// `prompt_files` (and, separately, its routed `context` documents) may
/// contribute to the system prompt.
///
/// Sized as the brief budget in
/// `docs/spec/runtime/orchestration/alignment.md` — 10,000 provider-billed
/// tokens — measured in codepoints as a cheap, tokenizer-free upper bound. One
/// token typically spans several codepoints for the encodings in use, so a
/// codepoint count is never smaller than the true token count: the clamp may cut
/// earlier than the budget strictly requires, never later.
///
/// Applied **where the prompt is spent** (assembly), never by refusing to load
/// the file. Refusing the read costs the company the whole document; clamping at
/// assembly costs only its tail.
///
/// [brief]: https://docs/spec/runtime/orchestration/alignment.md
pub const PROMPT_FILE_BUDGET_CHARS: usize = 10_000;

/// Built-in capability tier names selectable in `[plan].name` (issue #108). The
/// name → budget-map table lives in
/// [`plan_named`](crate::harness::capability_budget::plan_named); this list is
/// what manifest validation checks a name against.
pub const PLAN_NAMES: [&str; 4] = ["free", "starter", "pro", "unlimited"];

/// Budget windows selectable in `[plan].period` (issue #108).
pub const PLAN_PERIODS: [&str; 2] = ["daily", "monthly"];

/// The on-disk definition of a Company.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CompanyManifest {
    /// Company-level identity; seeds the Charter.
    pub company: Company,
    /// The roster. Renamed from the `[[agent]]` array-of-tables.
    #[serde(default, rename = "agent")]
    pub agents: Vec<Agent>,
    /// Internal group chats between the human and desks of agents. Renamed from
    /// the `[[group_chat]]` array-of-tables.
    #[serde(default, rename = "group_chat")]
    pub group_chats: Vec<GroupChat>,
    /// Third-party integrations to prioritize wiring, as intent — never
    /// secrets. Renamed from the `[[connection]]` array-of-tables.
    #[serde(default, rename = "connection")]
    pub connections: Vec<Connection>,
    /// Per-tenant MCP tool servers exposed to the company's agents (issue #50).
    /// Declarative intent — an HTTP endpoint, a tool allow/deny list, and an
    /// optional *named* secret key — **never** inline credentials. Renamed from
    /// the `[[mcp_server]]` array-of-tables.
    #[serde(default, rename = "mcp_server")]
    pub mcp_servers: Vec<McpServer>,
    /// Which workflow graphs (under the company's `workflows/` directory) to
    /// enable. The graphs themselves live in their own files, not here.
    #[serde(default)]
    pub workflows: Workflows,
    /// The company's human collaborators — who bootstraps admin access.
    #[serde(default)]
    pub users: Users,
    /// Brain selection.
    #[serde(default)]
    pub brain: Brain,
    /// The named execution engines this company's agents run on. Renamed from
    /// the `[[harness]]` array-of-tables.
    ///
    /// Empty (the default) means one implicit `built_in` harness on the
    /// company-level [`inference`](Self::inference) — resolve through
    /// [`effective_harnesses`](Self::effective_harnesses) rather than reading
    /// this field, so the implicit case is never forgotten.
    #[serde(default, rename = "harness")]
    pub harnesses: Vec<Harness>,
    /// Per-tenant Bring-Your-Own-Key inference routing (issue #56). Declarative
    /// intent — a provider kind, an OpenAI-compatible `base_url`, an optional
    /// *named* secret key (`api_key_secret`), and an abstract-tier → model map.
    /// **Never** an inline credential. Absent (the default) keeps the managed
    /// hosted brain. An anchor of its own, kept append-only.
    #[serde(default)]
    pub inference: Inference,
    /// Channel adapters, keyed by channel name.
    #[serde(default)]
    pub channels: BTreeMap<String, ChannelConfig>,
    /// Company-wide tool grants.
    #[serde(default)]
    pub tools: Tools,
    /// Default approval policy.
    #[serde(default)]
    pub policy: Policy,
    /// tiny.place going-public configuration.
    #[serde(default)]
    pub place: Place,
    /// Hard spend ceiling.
    #[serde(default)]
    pub budget: Budget,
    /// Per-tenant capability tier plan (issue #108) — a token budget per exec
    /// tool namespace, gating the `shell`/`code`/`web`/`subagent` families by the
    /// company's period token spend. Absent (the default) leaves gating off. This
    /// is a distinct axis from `[policy].mode` (autonomy) and an agent's `tier`
    /// (cognition) — a plan bounds *cost of capability*, not trust or model.
    #[serde(default)]
    pub plan: Plan,
    /// Cron-driven prompts. Renamed from the `[[schedule]]` array-of-tables.
    #[serde(default, rename = "schedule")]
    pub schedules: Vec<Schedule>,
    /// How this company relates to the global baseline ([`crate::globals`]).
    #[serde(default)]
    pub globals: Globals,
}

/// `[globals]` — this company's relationship to the global baseline.
///
/// The roster, workflows and skills are a floor every company gets whichever
/// vertical it started from, so the only thing left to configure for those is
/// what this company does *not* want. Replacing a global needs no entry here:
/// a company definition of the same id supersedes the global one on its own.
///
/// The tool belt (`[tools].default_allow`) is the one part of the baseline
/// that is a *default*, not a floor: it is what a company with no `[tools]`
/// section of its own starts with, never a minimum re-granted underneath one
/// — see [`crate::globals`].
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Globals {
    /// Globals this company drops outright, as `<kind>:<id>` keys — e.g.
    /// `agent:researcher`, `workflow:weekly_review`, `skill:meeting-brief`.
    ///
    /// Validated against what the baseline actually carries: an entry naming
    /// nothing is a manifest error, because the alternative is an opt-out the
    /// operator wrote, believed, and silently never got. The kinds are
    /// [`crate::globals::DISABLE_KINDS`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub disable: Vec<String>,
}

/// `[company]` — the seed of the Charter.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Company {
    /// Display name.
    pub name: String,
    /// What the company produces.
    #[serde(default)]
    pub output: Option<String>,
    /// The one thing the human owns.
    #[serde(default)]
    pub human_role: Option<String>,
    /// tiny.place `@handle`; only used when `[place].discoverable = true`.
    #[serde(default)]
    pub handle: Option<String>,
    /// Company logo as a self-contained data:image/... URL (issue: operator-set brand logo).
    #[serde(default)]
    pub logo_url: Option<String>,
}

/// A `[[agent]]` roster entry.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Agent {
    /// snake_case, unique within the roster.
    pub id: String,
    /// Human-readable job title.
    pub role: String,
    /// The display name an operator gave this teammate, when it has one
    /// (issue #1105).
    ///
    /// Set only for an operator-added (overlay) teammate, whose name the
    /// console shows on the DM header, subtitle and composer; a manifest
    /// `[[agent]]` is addressed by its role and leaves this `None`. Carried
    /// here so [`persona_prompt`](crate::company::prompt::persona_prompt) can tell the
    /// model the name the interface is already calling it — without it the
    /// agent denies being the person the console names.
    ///
    /// `#[serde(skip)]` deliberately: this is an in-memory carrier filled by
    /// the roster build, not a manifest key. Making it authorable would mean
    /// adding it to `AgentFile` too (`crate::company::agent_file`), or a
    /// `name` written in an `agents/*.toml` would be silently ignored while
    /// the same key worked in `company.toml`.
    #[serde(skip)]
    pub name: Option<String>,
    /// What this agent does.
    #[serde(default)]
    pub description: Option<String>,
    /// Cognition tier hint; never selects a model.
    #[serde(default)]
    pub tier: Option<String>,
    /// Which `[[harness]]` this agent runs its turns on, by id.
    ///
    /// `None` means the harness marked `default = true`. Deliberately separate
    /// from [`tier`](Self::tier): a tier names a *workload* and is resolved
    /// against whatever provider the harness turns out to use, whereas this
    /// picks the engine and the credential. An agent can keep its tier while
    /// moving between harnesses.
    #[serde(default)]
    pub harness: Option<String>,
    /// A model hint forwarded to this agent's ACP harness for this agent's
    /// own turns, overriding that `[[harness]].acp.model` when both are set
    /// (issue #1245's per-agent follow-up).
    ///
    /// Not a credential, for the same reason [`AcpHarness::model`] is not
    /// one — the ACP agent already holds its own. Meaningful only when this
    /// agent resolves to an `acp` harness with `transport = "local"`;
    /// validation rejects it on a `built_in`-harness agent rather than
    /// silently ignoring it, matching the harness-level field's own
    /// doctrine. Two agents sharing one `local` acp harness process still
    /// share the subprocess — the override steers that agent's own ACP
    /// *session* (`session/set_config_option`), not the process env.
    #[serde(default)]
    pub model: Option<String>,
    /// Tool grant globs, intersected with `[tools].allow`.
    ///
    /// Three distinct states, made representable by issue #1804 (epic #1817,
    /// Rung 2 — a standing grant that is real and explicit):
    ///
    /// * `None` — **inherit** the company's standard grant (the full
    ///   `[tools].allow`). This is the default, and how every record written
    ///   before #1804 (which had no `tools` key, or a `tools = []`) deserializes,
    ///   so promoting the field changes nothing for an existing manifest.
    /// * `Some(vec![])` — an **explicit, deliberate no-tools** grant: this
    ///   teammate reaches nothing. Newly reachable in #1804; before it, an empty
    ///   list was indistinguishable from an absent one and both meant "standard".
    /// * `Some(globs)` — **narrow** to the listed globs, intersected with
    ///   `[tools].allow` at roster-build time (narrow-only, never a widen).
    ///
    /// `skip_serializing_if = "Option::is_none"` keeps a standard-grant teammate
    /// serializing exactly as it did before this field was optional (no `tools`
    /// key), so no existing on-disk record moves.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<String>>,
    /// Desks this agent may hand work on to (issue #176).
    ///
    /// Empty (the default) means **no delegation tools at all** — the behaviour
    /// every manifest had before this field existed, and the reason adding it is
    /// a no-op for an existing company. A non-empty list wires
    /// `spawn_task` + `delegate_to_desk` + `delegate_to_teammate` (issue #884)
    /// onto this agent (never the orchestrator's roster/workflow/lifecycle
    /// authority), narrows `delegate_to_desk` to the desks named here, and lets
    /// `delegate_to_teammate` reach any member of any desk this agent sits on
    /// (deliberately unconditional — the enable switch is opting in at all, not
    /// which desk is named) plus every member of the desks named here. `"*"` is
    /// a wildcard for "every desk the company has" on both tools.
    ///
    /// Entries are **desk** ids or names, not teammate ids: desks are
    /// OpenCompany's delegation address space, and `delegate_to_desk` already
    /// resolves its target that way — `delegate_to_teammate` reads the same
    /// list and expands each desk to its members rather than taking teammate
    /// ids directly. Deliberately a field of its own rather than more
    /// [`tools`](Self::tools) grant globs — that vocabulary feeds the
    /// capability-namespace math, and a desk id is not a namespace.
    ///
    /// This is simultaneously the per-member enable switch and the capability
    /// budget: what a member may reach is stated once, in the manifest, rather
    /// than inferred from who happens to lead which desk.
    #[serde(default)]
    pub delegates_to: Vec<String>,
    /// Workspace documents routed into this agent's system prompt.
    ///
    /// The manifest half of *context routing*
    /// (`docs/spec/runtime/orchestration/alignment.md`): which working
    /// documents this role is told to reason from. Context is authority, so it
    /// is stated per role rather than given to everyone — and the exclusions
    /// carry as much weight as the entries, because a role that weighs evidence
    /// must not be handed unevidenced text beside it.
    ///
    /// Paths are relative to the company workspace root. A named document that
    /// does not exist is skipped; one that is oversized or not valid UTF-8 is an
    /// error, because silently dropping it yields a role whose prompt was
    /// written around a document it never received.
    ///
    /// `None` (an omitted `context` key) means this agent takes the
    /// per-tier default once the routing layer lands. `Some(vec![])` (an
    /// explicit `context = []`) means the role gets the universal document
    /// and nothing else, distinct from taking the default — see
    /// `docs/spec/runtime/orchestration/alignment.md`. `Option<Vec<String>>`
    /// rather than a defaulted `Vec<String>` is what makes that distinction
    /// representable at all: a plain `Vec` with `#[serde(default)]` cannot
    /// tell an omitted key from an explicit empty list apart, since both
    /// deserialize to the same empty vec.
    ///
    /// The **dynamic** half of the prompt-context pair: these are live
    /// operator-owned documents, re-read on every roster rebuild and placed last
    /// in the system prompt, after the static
    /// [`prompt_files`](Self::prompt_files). Documents are resolved by
    /// `harness::context_routing` before the (synchronous) agent build, and a
    /// change to one moves the roster fingerprint so it reaches the next turn
    /// rather than the next restart.
    ///
    /// An entry is either a bare path (routes the document into the prompt,
    /// read-only — the pre-existing shorthand) or `{ path, access = "write" }`,
    /// which additionally grants this agent `workspace_write`/`workspace_create`
    /// on that path. See [`ContextEntry`] and [`Agent::write_scope`].
    #[serde(default)]
    pub context: Option<Vec<ContextEntry>>,
    /// Per-agent daily spend cap in USD.
    #[serde(default)]
    pub budget_usd_daily: Option<f64>,
    /// A custom system-prompt body for this role, appended to the generated
    /// persona sentence rather than replacing it.
    ///
    /// Appended, not substituted, because the generated line is what binds the
    /// agent to *this* company and *this* role — an agent that replaced it would
    /// lose the identity framing that makes it answer as the Copywriter at Acme
    /// instead of falling back to the runtime's own assistant persona. What an
    /// operator wants here is the working instruction ("write in the brand's
    /// voice, never the agency's"), which is additive to that framing.
    ///
    /// Available on a `[[agent]]` entry as well as an `agents/<id>.toml` file:
    /// one field, one consumer, so adopting a prompt does not require adopting
    /// the bundle layout first.
    #[serde(default)]
    pub prompt: Option<String>,
    /// Checked-in documents folded into this agent's system prompt, as paths
    /// relative to the company bundle's `agents/` directory.
    ///
    /// The **static** half of the prompt-context pair. These are version
    /// controlled beside the agent definition and never change between two
    /// turns of a run, which is why they are read once at manifest load and
    /// placed early in the prompt where the cache prefix stays stable. The
    /// dynamic half is [`context`](Self::context): live workspace documents,
    /// re-read per roster rebuild and placed last.
    ///
    /// A path that escapes `agents/`, or names a file that does not exist, is a
    /// **validation error** rather than a skipped entry. This is deliberately
    /// stricter than `context`'s missing-document rule, and for a reason the two
    /// do not share: a workspace document is operator-owned live state that may
    /// legitimately not exist yet, while a `prompt_files` entry names a file in
    /// the same commit as the agent that references it. A typo there yields a
    /// role whose prompt was written around a briefing it silently never
    /// received.
    #[serde(default)]
    pub prompt_files: Vec<String>,
    /// The `(path, body)` pairs [`prompt_files`](Self::prompt_files) resolved
    /// to, filled by the bundle loader at manifest-load time.
    ///
    /// Carried on the manifest rather than read at prompt-assembly time because
    /// `harness::build::build_agent` is synchronous and runs on every roster
    /// rebuild; doing the file I/O there would put a disk read behind a lock on
    /// a hot path, to load bytes that cannot have changed since the manifest was
    /// parsed. `#[serde(skip)]` keeps it out of the on-disk record — it is
    /// derived state, and a persisted copy would go stale against the bundle.
    #[serde(skip)]
    pub prompt_files_resolved: Vec<(String, String)>,
    /// This role's epistemic classes, gating which routed documents it may be
    /// told. See [`PROMPT_CLASSES`] for the closed set and what each excludes.
    ///
    /// Empty (the default) is *unclassified*: no exclusion, the correct default
    /// for an ordinary teammate. A company that wants an exclusion states it
    /// here rather than having it guessed from the free-text `role`.
    #[serde(default)]
    pub classes: Vec<String>,
    /// Declared ledgers this agent may reach through the ledger tools, with
    /// per-ledger read/record access.
    ///
    /// `None` (the default, omitted key) means **unrestricted** — every ledger
    /// the company has, at `record` access — which is the tool surface every
    /// agent had before this field existed, so adding it is a no-op for an
    /// existing company. `Some(vec![])` restricts the agent to no ledgers at
    /// all, distinct from taking the default; `Option<Vec<LedgerGrant>>` rather
    /// than a defaulted `Vec` is what makes that distinction representable, the
    /// same reasoning as [`context`](Self::context).
    ///
    /// This is the **visibility and read/record** half of ledger access.
    /// [`LedgerSpec::writers`](crate::ledger::LedgerSpec::writers) stays the
    /// authoritative check for whether a `record`/`close` call actually lands —
    /// declaring `access = "record"` here for a built-in ledger whose writers
    /// exclude this agent is a manifest validation error (the two must not
    /// silently disagree); for a company-declared ledger, which may not exist
    /// yet when the manifest is validated, the same conflict surfaces as an
    /// ordinary tool refusal at call time.
    #[serde(default)]
    pub ledgers: Option<Vec<LedgerGrant>>,
    /// Whether this agent may declare a new ledger with `define_ledger`.
    ///
    /// Defaults to `true` — declaring an axis a company discovers it needs
    /// while running is deliberately unrestricted by default (see
    /// `docs/spec/runtime/ledgers.md`), and every manifest written before this
    /// field existed relied on that. Set `false` to keep a narrow role from
    /// growing the company's ledger registry.
    #[serde(default = "default_true")]
    pub can_declare_ledgers: bool,
    /// Whether this teammate came from the global baseline ([`crate::globals`])
    /// rather than the company's own roster.
    ///
    /// Provenance, not configuration: no author writes it, and the merge sets it
    /// on every teammate it appends. It exists because a manifest is serialized
    /// back into the store with the merged roster in it, so without a marker a
    /// global teammate becomes indistinguishable from one the company wrote —
    /// and the baseline could then never be updated, retired, or opted out of
    /// for that company again. With it,
    /// [`apply_globals`](CompanyManifest::apply_globals) is idempotent: it drops
    /// every previously-merged global and re-appends the current baseline, so a
    /// company picks up baseline changes and honours `[globals].disable` on the
    /// very next read.
    #[serde(default, skip_serializing_if = "is_false")]
    pub global: bool,
}

/// `#[serde(skip_serializing_if)]` predicate: keeps `global = false` — which is
/// every hand-authored teammate — out of the serialized manifest.
fn is_false(value: &bool) -> bool {
    !*value
}

/// One [`Agent::context`] entry.
///
/// A bare TOML string (`"brand/Voice.md"`) deserializes as [`Self::Path`], read
/// access, matching every manifest written before write access existed. The
/// table form (`{ path = "...", access = "write" }`) is [`Self::Detailed`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ContextEntry {
    /// Read access, the pre-existing shorthand.
    Path(String),
    /// Explicit access, most commonly `write`.
    Detailed {
        path: String,
        #[serde(default)]
        access: ContextAccess,
    },
}

impl ContextEntry {
    /// The workspace-relative path, whichever form declared it.
    pub fn path(&self) -> &str {
        match self {
            ContextEntry::Path(path) => path,
            ContextEntry::Detailed { path, .. } => path,
        }
    }

    /// This entry's access — `Read` unless a `Detailed` form says otherwise.
    pub fn access(&self) -> ContextAccess {
        match self {
            ContextEntry::Path(_) => ContextAccess::Read,
            ContextEntry::Detailed { access, .. } => *access,
        }
    }
}

impl From<&str> for ContextEntry {
    fn from(path: &str) -> Self {
        ContextEntry::Path(path.to_string())
    }
}

impl From<String> for ContextEntry {
    fn from(path: String) -> Self {
        ContextEntry::Path(path)
    }
}

/// Whether a routed [`ContextEntry`] is read-only or additionally writable.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextAccess {
    /// Routed into the prompt; no write grant on this path.
    #[default]
    Read,
    /// Routed into the prompt, and this exact path is in the agent's
    /// `workspace_write`/`workspace_create` scope.
    Write,
}

/// One [`Agent::ledgers`] entry: a ledger slug and this agent's access to it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LedgerGrant {
    /// The ledger's slug, as declared or built in — not validated against the
    /// registry at manifest-load time, since a company-declared ledger may not
    /// exist yet (the same reasoning as `Agent::context`'s missing-document
    /// rule).
    pub name: String,
    /// This agent's access to it.
    #[serde(default)]
    pub access: LedgerAccess,
}

/// Read or record access to one ledger.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LedgerAccess {
    /// `list_ledgers` and `read_ledger` only.
    #[default]
    Read,
    /// Read, plus `record_entry` and `close_entry` — still subject to
    /// [`LedgerSpec::writers`](crate::ledger::LedgerSpec::writers).
    Record,
}

impl Agent {
    /// This agent's `workspace_write`/`workspace_create` scope.
    ///
    /// `None` means **unconfined** — every path in the company's tree, which is
    /// the behaviour every agent had before per-path write access existed (see
    /// `src/harness/workspace_tools.rs`), so a manifest that declares no
    /// `access = "write"` entry is unaffected by this field's existence.
    /// `Some(paths)` — returned as soon as `context` names at least one write
    /// entry — confines `workspace_write`/`workspace_create` to exactly those
    /// paths, plus this agent's own `agents/<id>/` home, which the workspace
    /// tools always allow regardless of this scope.
    pub fn write_scope(&self) -> Option<Vec<String>> {
        let entries = self.context.as_ref()?;
        let paths: Vec<String> = entries
            .iter()
            .filter(|entry| entry.access() == ContextAccess::Write)
            .map(|entry| entry.path().to_string())
            .collect();
        if paths.is_empty() { None } else { Some(paths) }
    }

    /// This agent's declared access to ledger `slug`, or `None` if it may not
    /// reach it at all.
    ///
    /// An omitted `ledgers` key answers `Some(Record)` for every slug —
    /// unrestricted, matching the tool surface before this field existed.
    pub fn ledger_access(&self, slug: &str) -> Option<LedgerAccess> {
        match &self.ledgers {
            None => Some(LedgerAccess::Record),
            Some(grants) => grants
                .iter()
                .find(|grant| grant.name.eq_ignore_ascii_case(slug.trim()))
                .map(|grant| grant.access),
        }
    }
}

/// The `[[agent]].tier` value that marks a roster's orchestrator.
pub const ORCHESTRATOR_TIER: &str = "orchestrator";

/// Which teammate acts as the company's orchestrator: the first agent tagged
/// `tier = "orchestrator"`, else the first agent declared, else `None` for an
/// empty roster.
///
/// Lives here, ungated, because two very different readers need the same
/// answer. The harness reads it to decide who gets the delegating-orchestrator
/// persona and tools (`crate::harness::orchestrator::orchestrator_id`, which
/// delegates here). The console's agent detail route reads it to tell an
/// operator whether the agent they opened is the orchestrator or a worker
/// (issue #264) — and that route ships in the default build, where the harness
/// does not compile at all.
///
/// The fallback to the first agent is not a nicety: a company that tags nobody
/// still has an orchestrator, so a console that reported "worker" for every
/// teammate on such a roster would be wrong about all of them.
pub fn orchestrator_id(agents: &[Agent]) -> Option<&str> {
    agents
        .iter()
        .find(|agent| agent.tier.as_deref() == Some(ORCHESTRATOR_TIER))
        .or_else(|| agents.first())
        .map(|agent| agent.id.as_str())
}

/// A `[[group_chat]]` entry — a named conversation with a desk of agents.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GroupChat {
    /// snake_case, unique within the manifest's group chats.
    pub id: String,
    /// Human-readable chat name, e.g. "Creative studio".
    pub name: String,
    /// What the chat is for.
    #[serde(default)]
    pub description: Option<String>,
    /// Agent ids in this chat; each must exist in the roster.
    #[serde(default)]
    pub members: Vec<String>,
    /// This desk's tool ceiling: the middle level of the three-level narrowing
    /// `[tools].allow ∩ desk.tools ∩ [[agent]].tools`.
    ///
    /// A desk is how a company expresses a department — a finance desk, a
    /// creative desk — so it is the natural place to say "nobody on this desk
    /// reaches the web", once, instead of repeating the restriction on every
    /// member and hoping the next member added inherits it.
    ///
    /// Empty (the default) narrows **nothing**, which makes this field a no-op
    /// for every manifest written before it existed.
    ///
    /// A teammate sitting on several desks takes the **union** of their
    /// ceilings before the intersection with the company grant. Union rather
    /// than intersection because desks are additive memberships: joining the
    /// growth desk is how a marketer gains the ad tools, and an intersection
    /// would make each extra desk silently *remove* capability — so adding
    /// someone to a desk could break the job they already did. See
    /// [`agent_scoped_grants`](crate::runtime::builder::agent_scoped_grants).
    #[serde(default)]
    pub tools: Vec<String>,
}

/// A `[[connection]]` entry — an integration to prioritize wiring. This is
/// declarative intent (provider + scopes + why), never credentials.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Connection {
    /// Provider id, e.g. `slack`, `gmail`, `github`.
    pub provider: String,
    /// `low` | `medium` | `high`; how much to prioritize wiring it.
    #[serde(default)]
    pub priority: Option<String>,
    /// OAuth scopes the company expects to need.
    #[serde(default)]
    pub scopes: Vec<String>,
    /// Why the company wants this connection.
    #[serde(default)]
    pub reason: Option<String>,
}

/// A `[[mcp_server]]` entry — a remote MCP tool server the company's agents may
/// reach through the generic MCP bridge tools (issue #50).
///
/// This is declarative intent, shaped like [`Connection`]: it names an HTTP
/// endpoint and (optionally) which remote tools to allow, but it **never**
/// carries a credential. When a server needs auth, `auth_secret` names a
/// [`SecretStore`](crate::ports::SecretStore) key holding the token, which the
/// operator writes through the console (write-only). Hosted v1 supports the
/// **HTTP transport only** — a `command` (stdio/subprocess) server is rejected
/// by [`validate`](super::CompanyManifest::validate).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct McpServer {
    /// Stable slug used by the bridge tools and the console; unique per company.
    pub name: String,
    /// MCP endpoint URL. Must be `http(s)://` — the only transport hosted v1
    /// supports.
    #[serde(default)]
    pub endpoint: String,
    /// Optional human-readable description shown in the console + bridge output.
    #[serde(default)]
    pub description: Option<String>,
    /// A stdio/subprocess command. **Unsupported in hosted v1** — its presence
    /// is a validation error (agents run in a shared multi-tenant container;
    /// spawning per-tenant subprocesses is out of scope). Kept as a field so the
    /// error can name the problem instead of a confusing "missing endpoint".
    #[serde(default)]
    pub command: Option<String>,
    /// Exact remote tool names to allow. Empty means all remote tools are
    /// allowed unless listed in `disallowed_tools`.
    #[serde(default)]
    pub allowed_tools: Vec<String>,
    /// Exact remote tool names to always hide/block (takes precedence).
    #[serde(default)]
    pub disallowed_tools: Vec<String>,
    /// Exact remote tool names on this server the operator declares **read-only**
    /// (issue #1124): they read and change nothing, so a bridge call to one need
    /// not park under `auto` the way calling *through* a server otherwise does.
    ///
    /// This is an operator declaration, not a claim read off the remote — there
    /// is no client-side annotation to trust — so an undeclared tool always
    /// gates. Independent of `allowed_tools`/`disallowed_tools`: it says nothing
    /// about whether a tool is *exposed*, only how a call to it is priced.
    #[serde(default)]
    pub read_only_tools: Vec<String>,
    /// Per-request timeout in seconds.
    #[serde(default = "default_mcp_timeout_secs")]
    pub timeout_secs: u64,
    /// Whether this server is exposed to agents. Defaults to on.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Optional name of the [`SecretStore`](crate::ports::SecretStore) key that
    /// holds this server's outbound credential. Names a key — never the token.
    /// When unset, the runtime resolves the canonical per-server key
    /// (`mcp/<name>/auth`) written by the console.
    #[serde(default)]
    pub auth_secret: Option<String>,
}

fn default_mcp_timeout_secs() -> u64 {
    super::mcp::DEFAULT_TIMEOUT_SECS
}

fn default_true() -> bool {
    true
}

/// The default ceiling on concurrent in-flight workflow runs per company
/// (issue #401), applied when `[workflows].max_in_flight_runs` is omitted.
///
/// Deliberately well above 1: a running workflow's agent node can itself call
/// the `run_workflow` tool, so the parent run holds a slot while a child begins.
/// A ceiling of 1 would refuse that legitimate nesting on the first hop, which
/// is why the field's validation floor is 1 but its default is generous.
pub const DEFAULT_MAX_IN_FLIGHT_RUNS: usize = 8;

fn default_max_in_flight_runs() -> usize {
    DEFAULT_MAX_IN_FLIGHT_RUNS
}

/// `[workflows]` — references to the workflow graphs to enable. The graphs live
/// as separate files under the company's `workflows/` directory.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Workflows {
    /// Workflow ids to enable, each a `workflows/<id>.toml` graph file.
    #[serde(default)]
    pub enabled: Vec<String>,
    /// The most workflow runs this company may have executing at once
    /// (issue #401). Every run — manual, scheduled, gate-resume, or one an
    /// orchestrator agent starts — is admitted through the same choke point and
    /// counts against this ceiling; a run over it is refused, never queued.
    /// Defaults to [`DEFAULT_MAX_IN_FLIGHT_RUNS`]; validation requires `>= 1`.
    #[serde(default = "default_max_in_flight_runs")]
    pub max_in_flight_runs: usize,
}

impl Default for Workflows {
    fn default() -> Self {
        Self {
            enabled: Vec::new(),
            max_in_flight_runs: DEFAULT_MAX_IN_FLIGHT_RUNS,
        }
    }
}

/// `[users]` — the company's human collaborators.
///
/// Access is invite-only, which raises a bootstrap question: someone has to
/// send the first invite, and there is no operator token to do it with (see
/// `docs/spec/runtime/config.md`). This is the answer. Addresses listed here
/// are treated as standing admin invites, so the manifest — which is the
/// company's definition, under version control — is the root of trust for who
/// may administer it.
///
/// ```toml
/// [users]
/// mode = "email"                   # email (default) | wallet | none
/// admins = ["ada@example.com"]     # email mode
/// wallets = ["7xKXtg2CW87d97…"]    # wallet mode
/// ```
///
/// Listing an address does not create an account. It makes that address
/// *eligible* to log in, at which point redeeming a magic link mints the user
/// as an admin. Removing an address from the manifest stops it bootstrapping
/// again but does not delete an account it already created — use the admin
/// routes for that.
///
/// `mode` picks *how* people sign in — see
/// [`AuthMode`](crate::app::config::AuthMode) — and therefore which bootstrap
/// list is read. `admins` is read in `email` mode, `wallets` in `wallet` mode,
/// and neither in `none` mode, where there is no sign-in and the only account is
/// the implicit local owner. The host may override the mode for every company it
/// serves with `OPENCOMPANY_AUTH_MODE`; absent that, this is the answer.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Users {
    /// `email` (default) | `wallet` | `none`.
    ///
    /// Kept as a `String` here, like [`Brain::mode`], so that a manifest with an
    /// unknown mode still *parses* and validation can name the offending value
    /// in prosumer language instead of emitting a serde trace.
    #[serde(default = "default_auth_mode")]
    pub mode: String,
    /// Email addresses that may log in as admins without being invited first.
    /// Read in `email` mode.
    #[serde(default)]
    pub admins: Vec<String>,
    /// Base58 Ed25519 wallet addresses that may sign in as admins without being
    /// invited first. Read in `wallet` mode.
    ///
    /// The wallet equivalent of [`Self::admins`], with the same semantics
    /// exactly: listing an address makes it eligible, signing a challenge mints
    /// the admin, and removing it stops future bootstrapping without deleting an
    /// account it already created.
    #[serde(default)]
    pub wallets: Vec<String>,
}

impl Default for Users {
    fn default() -> Self {
        Self {
            mode: default_auth_mode(),
            admins: Vec::new(),
            wallets: Vec::new(),
        }
    }
}

fn default_auth_mode() -> String {
    "email".to_string()
}

/// `[brain]` — selects the `Brain` implementation.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Brain {
    /// `hosted` (default) | `sidecar`.
    #[serde(default = "default_brain_mode")]
    pub mode: String,
    /// Passed through to Medulla.
    #[serde(default)]
    pub max_passes: Option<u32>,
}

impl Default for Brain {
    fn default() -> Self {
        Self {
            mode: default_brain_mode(),
            max_passes: None,
        }
    }
}

fn default_brain_mode() -> String {
    "hosted".to_string()
}

/// A `[[harness]]` entry — one named execution engine the company's agents may
/// run their turns on.
///
/// A company declares a set of these and binds each agent to one with
/// [`Agent::harness`], so a single roster can span a cheap model, an expensive
/// one, and the operator's own Claude Code — the last needing no credential from
/// us at all.
///
/// Like [`McpServer`] and [`Inference`], this is declarative intent and **never**
/// carries a token: a `built_in` harness's credential is named by
/// `[harness.inference].api_key_secret`, and an `acp` harness holds its own.
///
/// The TOML shape is an array-of-tables with sub-tables:
///
/// ```toml
/// [[harness]]
/// id      = "embedded"
/// kind    = "built_in"
/// default = true
///
/// [harness.inference]      # attaches to the entry above
/// provider = "openrouter"
/// ```
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Harness {
    /// snake_case, unique within the company. Agents name this.
    pub id: String,
    /// One of [`HARNESS_KINDS`].
    #[serde(default = "default_harness_kind")]
    pub kind: String,
    /// Whether agents naming no harness run here. Exactly one entry must set
    /// it, whenever any `[[harness]]` is declared.
    #[serde(default)]
    pub default: bool,
    /// This harness's own inference routing. `built_in` only. Absent falls back
    /// to the company-level `[inference]` section.
    #[serde(default)]
    pub inference: Option<Inference>,
    /// How to reach the external agent. `acp` only.
    #[serde(default)]
    pub acp: Option<AcpHarness>,
}

fn default_harness_kind() -> String {
    DEFAULT_HARNESS_KIND.to_string()
}

impl Harness {
    /// The implicit harness a company with no `[[harness]]` block runs on: one
    /// `built_in` entry, marked default, inheriting the company-level
    /// `[inference]`.
    ///
    /// Synthesized rather than required in every manifest so that adding named
    /// harnesses changes nothing for a company that never asked for them —
    /// every bundle under `companies/` lands here.
    pub fn implicit() -> Self {
        Self {
            id: IMPLICIT_HARNESS_ID.to_string(),
            kind: DEFAULT_HARNESS_KIND.to_string(),
            default: true,
            inference: None,
            acp: None,
        }
    }

    /// The harness a teammate gets by naming a coding CLI this build knows how
    /// to drive, without any `[[harness]]` declaring it (issue #1245's
    /// detected-harness follow-up).
    ///
    /// A local ACP harness is a property of the **machine**, not of the
    /// company: whether `claude-agent-acp` is installed and signed in is
    /// answered by the desktop's own `acp::discovery` survey, and a
    /// version-controlled `company.toml` is the wrong place to record it —
    /// the same manifest is opened from a machine where the answer differs.
    /// So the manifest vocabulary ([`ACP_AGENTS`]) is treated as a set of ids
    /// that are *bindable without being declared*, and this synthesizes the
    /// harness a binding to one resolves to.
    ///
    /// Deliberately **never** `default`: which harness an unbound teammate
    /// runs on stays a blueprint decision, so nothing a machine happens to
    /// have installed can silently redirect a company's whole roster.
    ///
    /// Synthesized on demand rather than folded into
    /// [`effective_harnesses`](crate::company::CompanyManifest::effective_harnesses):
    /// a company that references none of these must produce **no** extra lanes
    /// and **no** extra `unavailable` entries, because
    /// `brain.rs` returns the plain engine when both are empty — and adding
    /// three phantom entries to every company would skip that path for all of
    /// them.
    ///
    /// A declared `[[harness]]` of the same id always wins; this is only ever
    /// the fallback for an id nothing declares.
    pub fn implicit_local(agent: &str) -> Self {
        Self {
            id: agent.to_string(),
            kind: "acp".to_string(),
            default: false,
            inference: None,
            acp: Some(AcpHarness {
                transport: "local".to_string(),
                agent: Some(agent.to_string()),
                runner: None,
                model: None,
            }),
        }
    }

    /// Whether `id` names a coding CLI this build can drive locally, and so is
    /// bindable even when no `[[harness]]` declares it. See
    /// [`implicit_local`](Self::implicit_local).
    pub fn is_implicit_local_id(id: &str) -> bool {
        ACP_AGENTS.contains(&id)
    }

    /// Whether this is the embedded loop — the only kind that consults
    /// `[inference]`.
    pub fn is_built_in(&self) -> bool {
        self.kind == "built_in"
    }
}

/// `[harness.acp]` — how to reach an external ACP agent.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct AcpHarness {
    /// One of [`ACP_TRANSPORTS`].
    #[serde(default)]
    pub transport: String,
    /// Which agent to spawn, one of [`ACP_AGENTS`]. `local` transport only.
    #[serde(default)]
    pub agent: Option<String>,
    /// Which registered runner holds this scope. `runner` transport only.
    #[serde(default)]
    pub runner: Option<String>,
    /// A model hint forwarded to the agent's own startup lever, when this
    /// build knows one for `agent` (issue #1245).
    ///
    /// Not a credential — the ACP agent already holds its own, which is the
    /// whole point of this harness kind — so this does not join
    /// `[harness.inference]`'s prohibition on `acp` harnesses. `local`
    /// transport only for now: the `runner` wire protocol does not carry it
    /// yet, so validation rejects it there rather than accepting and silently
    /// dropping it.
    #[serde(default)]
    pub model: Option<String>,
}

/// `[inference]` — per-tenant Bring-Your-Own-Key inference routing (issue #56).
///
/// This is declarative intent, shaped like [`McpServer`]: it names a provider
/// kind, an OpenAI-compatible `base_url`, and (optionally) which
/// [`SecretStore`](crate::ports::SecretStore) key holds the outbound
/// credential, but it **never** carries a token. When a provider needs auth,
/// `api_key_secret` names a key holding it, which the operator writes through
/// the console (write-only). An absent section (`provider = None`) keeps the
/// managed hosted brain.
///
/// The `models` table maps an abstract cognition tier (`chat-v1`,
/// `reasoning-v1`, `agentic-v1`, `vision-v1`) to a concrete provider model id
/// (e.g. `deepseek/deepseek-chat`). An unmapped tier passes through verbatim.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Inference {
    /// Provider kind — one of [`INFERENCE_PROVIDERS`]. `None` (absent section)
    /// keeps the managed hosted brain.
    #[serde(default)]
    pub provider: Option<String>,
    /// Base URL of the OpenAI-compatible chat-completions API. Required for
    /// `openai_compatible` and `ollama`; defaulted for `managed`/`openrouter`.
    #[serde(default)]
    pub base_url: Option<String>,
    /// The name of the [`SecretStore`](crate::ports::SecretStore) key holding
    /// this provider's outbound credential. Names a key — **never** the token.
    /// When unset, the runtime resolves the canonical key (`inference/key`)
    /// written by the console.
    #[serde(default)]
    pub api_key_secret: Option<String>,
    /// Abstract-tier → concrete provider model id. An unmapped tier passes
    /// through to the provider unchanged.
    #[serde(default)]
    pub models: BTreeMap<String, String>,
}

impl Inference {
    /// Whether this manifest section names a provider — i.e. it meaningfully
    /// configures inference (an absent `[inference]` leaves `provider` `None`).
    pub fn is_set(&self) -> bool {
        self.provider
            .as_deref()
            .is_some_and(|p| !p.trim().is_empty())
    }
}

/// A `[channels.*]` entry.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ChannelConfig {
    /// Whether the channel is enabled. Defaults to on for `operator`.
    #[serde(default)]
    pub enabled: Option<bool>,
    /// Delegating provider, e.g. `openhuman`.
    #[serde(default)]
    pub provider: Option<String>,
}

/// `[tools]` — company-wide tool grants.
///
/// `PartialEq` so `runtime::builder::carry_tool_grants_override` can ask the one
/// question the seed-wins rule turns on: did version control speak about
/// `[tools]` since the operator's console grant was written (issue #1796)?
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Tools {
    /// `openhuman` (default) | `builtin`.
    #[serde(default = "default_tool_provider")]
    pub provider: String,
    /// Company-wide grant globs; agents intersect with this.
    #[serde(default)]
    pub allow: Vec<String>,
    /// SSRF allowlist for the `web` tool namespace (Cell A). Empty (default) is
    /// *open mode* — all public hosts allowed — while private/loopback/
    /// link-local/metadata IPs are always rejected by OpenHuman's `url_guard`.
    /// A non-empty list is strict (only those hosts + subdomains); `"*"` is an
    /// explicit allow-all-public wildcard.
    #[serde(default)]
    pub web_allowed_domains: Vec<String>,
    /// Per-tenant Composio tools (issue #110, Cell D): the `[tools.composio]`
    /// sub-section. The `toolkits` allowlist narrows which Composio toolkits the
    /// agent may target (Gmail / Slack / GitHub, …). Empty (the default) DEFERS
    /// to the backend's own server-enforced allowlist — *open mode*, mirroring
    /// [`web_allowed_domains`]; a non-empty list is strict client-side narrowing
    /// (a toolkit outside it is rejected before any network call). Independent of
    /// the `composio` grant: the grant admits the tool family, this narrows what
    /// it can reach.
    #[serde(default)]
    pub composio: ComposioTools,
    /// Per-company **daily** ceiling on metered `web_search` calls (issue
    /// #238), counted per UTC day across every agent of the company.
    ///
    /// Absent (the default) uses [`DEFAULT_SEARCH_DAILY_CALLS`]. `0` disables
    /// searching outright while leaving the grant in place, which is the honest
    /// way to pause spend without editing `allow`. Reaching the cap makes the
    /// tool return a loud "search budget exhausted" error, never a silent drop
    /// — an agent that is told it is out of budget reports the constraint,
    /// whereas an agent handed an empty result invents citations.
    #[serde(default)]
    pub search_daily_calls: Option<u32>,
    /// How many levels deep one operator message's delegation chain may run
    /// (issue #176), counted in **hand-offs**: the orchestrator handing work to
    /// a desk lead is level 1, that lead handing a slice to a second desk is
    /// level 2.
    ///
    /// Absent (the default) uses [`DEFAULT_MAX_DELEGATION_DEPTH`]. `1` is the
    /// "recursion off" setting — it reproduces the pre-#176 depth cap exactly,
    /// where a dispatched desk agent could not re-delegate at all — and is the
    /// config gate a company reaches for when a chain is costing more than it
    /// returns. Valid values are `1..=4`; the ceiling is deliberately low
    /// because each level multiplies the turns one message can buy.
    ///
    /// Enforced **dynamically**, at the tool boundary, rather than by which
    /// tools were wired: belts are cached per roster and rebuilt rarely, so a
    /// member's tools are static while its depth is a property of the chain it
    /// is running inside.
    #[serde(default)]
    pub max_delegation_depth: Option<u8>,
}

/// Daily `web_search` call ceiling applied when `[tools].search_daily_calls` is
/// absent (issue #238).
///
/// Sized as a working day of research, not a hard product limit: at the managed
/// backend's ~$0.01/request list price this bounds an unattended runaway to
/// roughly $2/day/company while leaving a genuine multi-topic research session
/// (a handful of searches per question) comfortably inside it.
pub const DEFAULT_SEARCH_DAILY_CALLS: u32 = 200;

/// Delegation chain depth applied when `[tools].max_delegation_depth` is absent
/// (issue #176).
///
/// Two levels: the orchestrator hands work to a desk lead, and that lead may
/// hand one slice on to a second desk. That is the shape the issue asks for —
/// a lead that can bring in a specialist without going back through the CEO —
/// and it stops there because a third level buys little and costs a full extra
/// turn per branch on top of an already-multiplied fan-out.
///
/// The default is only reachable by a member the manifest opted in with
/// [`Agent::delegates_to`](crate::company::Agent::delegates_to); a company that
/// names nobody behaves exactly as it did before this existed, whatever this
/// number says.
pub const DEFAULT_MAX_DELEGATION_DEPTH: u8 = 2;

/// The inclusive bounds `[tools].max_delegation_depth` is validated against.
///
/// `1` disables recursion (the pre-#176 behaviour). `4` is the ceiling: a chain
/// deeper than that is indistinguishable from a runaway, and the per-turn
/// fan-out cap applies *per level*, so depth 5 admits `3^5` hand-offs from one
/// message.
pub const MAX_DELEGATION_DEPTH_BOUNDS: std::ops::RangeInclusive<u8> = 1..=4;

/// `[tools.composio]` — the per-tenant Composio toolkit allowlist (issue #110).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ComposioTools {
    /// Toolkit slugs the agent may target (e.g. `gmail`, `slack`, `github`).
    /// Empty defers to the backend's server-enforced allowlist (open mode);
    /// non-empty narrows strictly, client-side, before any network round-trip.
    #[serde(default)]
    pub toolkits: Vec<String>,
}

impl Default for Tools {
    fn default() -> Self {
        Self {
            provider: default_tool_provider(),
            // Grant the full tool belt by default: `*` covers files/docs/shell/
            // code/web/subagent, while `workspace.*` and the explicit
            // `workspace.write` grant cover the workspace read/write surface.
            // `media`/`composio` are listed literally because the `*` wildcard
            // deliberately excludes those two (real-money + per-tenant-
            // credential) namespaces. A company that wants a narrower belt
            // overrides `[tools].allow` explicitly.
            //
            // `search` is now part of the authored default belt so the
            // first-run setup flow can search without each generated agent
            // having to rediscover the capability.
            allow: crate::globals::default_tool_allow(),
            web_allowed_domains: Vec::new(),
            composio: ComposioTools::default(),
            search_daily_calls: None,
            max_delegation_depth: None,
        }
    }
}

fn default_tool_provider() -> String {
    "openhuman".to_string()
}

/// `[policy]` — the default `ApprovalGate` configuration.
///
/// `PartialEq` is load-bearing rather than incidental: `runtime::builder`
/// compares the previous boot's seed `[policy]` with this one's to decide
/// whether a console override survives the rebuild (issue #562).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Policy {
    /// `readonly` | `supervised` (default) | `auto` | `full`.
    ///
    /// **The default stays `supervised`, and issue #605 is the decision to keep
    /// it there rather than an unfinished flip.** #560 argued `auto` should
    /// become the shipped default, and #605 agrees on the destination — new
    /// companies do get `auto`. What it declines is delivering that by moving
    /// *this* value, because this value answers for every manifest ever parsed,
    /// not only for new ones. See [`PROVISIONED_POLICY_MODE`].
    ///
    /// Two things go wrong if it moves. The obvious one is that every company
    /// with no `[policy]` block silently widens on its next load. The other is
    /// not obvious and is worse: the persisted record stores this *defaulted*
    /// value, so a silent `company.toml` that parsed to `supervised` last boot
    /// parses to `auto` this boot, and `carry_policy_override` in
    /// [`crate::runtime::builder`] — which is `previous_seed == next_seed` over
    /// the whole block — reads that as version control having spoken and
    /// **discards the operator's console `[policy]` override**, including one
    /// that had tightened the tier. Nobody edited anything.
    ///
    /// So the tier a new company gets is written into its manifest at
    /// provisioning, explicitly, where an operator can read it. Existing
    /// companies are not re-tiered by a constant changing under them.
    #[serde(default = "default_policy_mode")]
    pub mode: String,
    /// Effect kinds that always park for approval regardless of amount, and
    /// regardless of tier — this list wins over `full`.
    ///
    /// A tool name is an effect kind (the harness projects one onto the other),
    /// so `["publish_artifact"]` and `["payment.send"]` are the same syntax at
    /// different segment counts. Matched by
    /// [`always_approve::matches`](crate::policy::always_approve::matches) on
    /// both approval paths (issue #684). Native effect kinds are open-ended, so
    /// configured entries are not restricted to this build's declared tools.
    ///
    /// Defaults to [`DEFAULT_ALWAYS_APPROVE`], which is empty — see there for
    /// why.
    #[serde(default = "default_always_approve")]
    pub always_approve: Vec<String>,
    /// Spends strictly under this many USD skip approval.
    #[serde(default)]
    pub auto_approve_under_usd: Option<f64>,
    /// How many hours a parked approval waits before it default-denies
    /// (issue #971). `None` takes the gate's
    /// [`DEFAULT_TTL_MILLIS`](crate::policy::DEFAULT_TTL_MILLIS), 24 hours.
    ///
    /// **Deliberately a bare `Option` with no serde default, and the absence is
    /// load-bearing.** The obvious alternative — `#[serde(default = "…")]`
    /// resolving to `Some(24)` at parse — walks straight into the trap
    /// documented on [`mode`](Self::mode) above. The persisted record stores
    /// the *defaulted* value, and `carry_policy_override` in
    /// [`crate::runtime::builder`] is `previous_seed == next_seed` over this
    /// whole block; so the day this default moves, every company with a silent
    /// manifest gets a seed that changed under it, and the rebuild discards the
    /// operator's console `[policy]` override as though version control had
    /// spoken. Nobody edited anything, and the feature that breaks is not this
    /// one.
    ///
    /// So `None` means "not configured" all the way through parse and persist,
    /// and the default is resolved exactly once, at
    /// [`ManifestApprovalGate::new`](crate::policy::ManifestApprovalGate::new).
    ///
    /// Skipped when absent so a manifest that never mentioned the knob
    /// serializes byte-identically to before this field existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_ttl_hours: Option<u64>,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            mode: default_policy_mode(),
            always_approve: default_always_approve(),
            auto_approve_under_usd: None,
            approval_ttl_hours: None,
        }
    }
}

fn default_policy_mode() -> String {
    "supervised".to_string()
}

fn default_always_approve() -> Vec<String> {
    DEFAULT_ALWAYS_APPROVE
        .iter()
        .map(|s| s.to_string())
        .collect()
}

/// `[place]` — tiny.place going-public configuration.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Place {
    /// Going public is opt-in; defaults to false.
    #[serde(default)]
    pub discoverable: bool,
    /// Skills feeding Agent Card generation.
    #[serde(default)]
    pub skills: Vec<Skill>,
}

/// A priced skill advertised on the company's Agent Card.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Skill {
    /// Skill identifier, e.g. `seo.audit`.
    pub id: String,
    /// Decimal USDC price string, e.g. `"25.00"`.
    pub price_usd: String,
    /// What the skill delivers.
    #[serde(default)]
    pub description: Option<String>,
}

/// `[budget]` — a hard ceiling across inference and x402 spend.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Budget {
    /// Monthly hard cap in USD.
    #[serde(default)]
    pub monthly_usd: Option<f64>,
}

/// `[plan]` — the company's capability tier plan (issue #108).
///
/// Declarative intent shaped like the other manifest sections: an optional
/// built-in tier `name` (`free` / `starter` / `pro` / `unlimited`), the budget
/// `period` (`daily` — the default — or `monthly`), and an explicit
/// `token_budgets` table mapping an exec tool namespace (`shell` / `code` /
/// `web` / `subagent`) to the tokens it may burn per period. The named tier
/// supplies a base map; `token_budgets` overrides/extends it. A namespace absent
/// from the effective map is denied outright — the map's key set *is* the
/// company's capability set. An absent `[plan]` leaves gating off entirely.
///
/// The budget is a **threshold over the company's total period token spend**, not
/// a per-namespace meter (usage samples carry no per-tool attribution): when
/// spend reaches a tier's budget, that tier's tools switch off. See
/// [`CapabilityPlan`](crate::harness::capability_budget::CapabilityPlan).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Plan {
    /// Built-in tier name, or `None` for a bare `token_budgets`-only plan.
    #[serde(default)]
    pub name: Option<String>,
    /// Budget window — `daily` (default) or `monthly`.
    #[serde(default = "default_plan_period")]
    pub period: String,
    /// Exec-namespace → tokens allowed per period. Overrides/extends the named
    /// tier's map; a gateable namespace absent here is denied.
    #[serde(default)]
    pub token_budgets: BTreeMap<String, u64>,
    /// Plan-level **total token ceiling** for the period (issue #188). The
    /// per-namespace `token_budgets` gate only *which exec tools* a turn may use
    /// (a soft roster trim — an exhausted namespace's tools drop, but the turn
    /// still runs on intrinsic tools and burns model tokens). This is the hard
    /// stop: once the company's total period spend reaches this ceiling, the
    /// harness **refuses to dispatch the turn at all** (no model call) until the
    /// period resets. `None` (the default) leaves the total gate off — only the
    /// per-namespace soft gate applies, byte-identical to pre-#188.
    #[serde(default)]
    pub total_tokens: Option<u64>,
}

impl Default for Plan {
    fn default() -> Self {
        // A manual impl (not `#[derive(Default)]`) so an absent `[plan]` section —
        // which `#[serde(default)]` fills via `Plan::default()`, NOT the per-field
        // `default_plan_period` — still carries the `daily` window, matching the
        // key-present-but-missing case. `is_set()` stays false regardless.
        Self {
            name: None,
            period: default_plan_period(),
            token_budgets: BTreeMap::new(),
            total_tokens: None,
        }
    }
}

impl Plan {
    /// Whether this section meaningfully configures a plan — a named tier, an
    /// explicit per-namespace budget, or a total-token ceiling (issue #188). An
    /// absent `[plan]` deserializes to the default (period only), which is *not*
    /// set, so gating stays off.
    pub fn is_set(&self) -> bool {
        self.name.as_deref().is_some_and(|n| !n.trim().is_empty())
            || !self.token_budgets.is_empty()
            || self.total_tokens.is_some()
    }
}

fn default_plan_period() -> String {
    "daily".to_string()
}

/// A `[[schedule]]` entry; becomes a `ScheduleFired` event.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Schedule {
    /// Standard 5-field cron expression.
    pub cron: String,
    /// Prompt delivered to the company when the schedule fires.
    pub prompt: String,
}

#[cfg(test)]
mod test {
    use super::*;

    /// The shared native vocabulary is exactly `GATEABLE_NAMESPACES` minus the
    /// third-party connection path (`composio`) and the raw-HTTP family the S2
    /// deflection governs (`web`).
    #[test]
    fn native_capability_vocabulary_is_gateable_minus_composio_and_web() {
        let native: std::collections::HashSet<&str> =
            native_capability_namespaces().into_iter().collect();
        let expected: std::collections::HashSet<&str> = GATEABLE_NAMESPACES
            .iter()
            .copied()
            .filter(|ns| *ns != "composio" && *ns != "web")
            .collect();
        assert_eq!(native, expected);
        assert!(!native.contains("composio"));
        assert!(!native.contains("web"));
    }

    /// `grants_confer_native` mirrors the harness wiring gate: the real-money
    /// `search`/`media` families need their explicit grant (a bare `*` confers
    /// neither), and every other native namespace rides the ordinary rule a `*`
    /// satisfies.
    #[test]
    fn grants_confer_native_mirrors_the_wiring_gate() {
        assert!(grants_confer_native(&["search".into()], "search"));
        assert!(!grants_confer_native(&["*".into()], "search"));
        assert!(!grants_confer_native(&["composio".into()], "search"));

        assert!(grants_confer_native(&["media".into()], "media"));
        assert!(!grants_confer_native(&["*".into()], "media"));

        assert!(grants_confer_native(&["*".into()], "shell"));
        assert!(grants_confer_native(&["shell".into()], "shell"));
        assert!(!grants_confer_native(&["search".into()], "shell"));
    }

    /// **T10 (issue #971).** A manifest that never mentions
    /// `approval_ttl_hours` parses to `None` and serializes without the key —
    /// byte-identical to a build that predates the field.
    ///
    /// This is not a serde-formatting nicety, it is the guard on
    /// `carry_policy_override`. That rule is `previous_seed == next_seed` over
    /// the whole `[policy]` block, so any value this field acquires at parse
    /// becomes part of the identity of a block nobody wrote — and the day the
    /// default moves, every silent manifest's seed changes under it and the
    /// operator's console `[policy]` override is discarded as though version
    /// control had spoken. The absence has to survive parse, persist and
    /// reload for that not to happen. See the field's own note.
    #[test]
    fn a_manifest_without_an_approval_ttl_round_trips_unchanged() {
        let silent: Policy = toml::from_str(
            r#"
            mode = "supervised"
            "#,
        )
        .expect("parse toml");
        assert_eq!(silent.approval_ttl_hours, None);

        // Byte-identical: the key is absent from the wire, not `null`.
        let json = serde_json::to_string(&silent).expect("serialize");
        assert!(
            !json.contains("approval_ttl_hours"),
            "a silent manifest must not gain the key on the wire: {json}"
        );

        // And the reload is `==` to the parse, which is the comparison
        // `carry_policy_override` actually runs.
        let back: Policy = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, silent);
        assert_eq!(
            serde_json::to_string(&back).expect("serialize"),
            json,
            "a persist/reload cycle must be a fixed point"
        );

        // A manifest that DOES configure it keeps the value across the same
        // cycle — the absence is meaningful, so presence must be too.
        let configured: Policy = toml::from_str(
            r#"
            mode = "supervised"
            approval_ttl_hours = 72
            "#,
        )
        .expect("parse toml");
        assert_eq!(configured.approval_ttl_hours, Some(72));
        let json = serde_json::to_string(&configured).expect("serialize");
        let back: Policy = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, configured);

        // The two are NOT equal, which is what makes the seed comparison able
        // to tell "operator configured a deadline" from "nobody said anything".
        assert_ne!(silent, configured);
    }

    /// Real-money `media` (issue #109) is granted ONLY by an explicit `media` /
    /// `media.*` grant — never by the catch-all `*`. This wildcard exclusion is
    /// the security property that keeps a broadly-permissioned company from
    /// accidentally handing its agents a paid image/video generator.
    #[test]
    fn media_grant_requires_explicit_namespace_not_wildcard() {
        assert!(grants_media_explicit(&["media".into()]));
        assert!(grants_media_explicit(&["media.image".into()]));
        assert!(grants_media_explicit(&["web.*".into(), "media".into()]));
        // The catch-all `*` must NOT grant media.
        assert!(!grants_media_explicit(&["*".into()]));
        assert!(!grants_media_explicit(&["web.*".into()]));
        assert!(!grants_media_explicit(&[]));
        // A substring match ("mediation") must not count as the media namespace.
        assert!(!grants_media_explicit(&["mediation".into()]));
    }

    /// Per-tenant `composio` (issue #110) is granted ONLY by an explicit
    /// `composio` / `composio.*` grant — never by the catch-all `*`. The tools
    /// reach third-party accounts over a tenant OAuth token, so a broadly-
    /// permissioned company must still opt into them by name.
    #[test]
    fn composio_grant_requires_explicit_namespace_not_wildcard() {
        assert!(grants_composio_explicit(&["composio".into()]));
        assert!(grants_composio_explicit(&["composio.gmail".into()]));
        assert!(grants_composio_explicit(&[
            "web.*".into(),
            "composio".into()
        ]));
        // The catch-all `*` must NOT grant composio.
        assert!(!grants_composio_explicit(&["*".into()]));
        assert!(!grants_composio_explicit(&["web.*".into()]));
        assert!(!grants_composio_explicit(&[]));
        // A substring match must not count as the composio namespace.
        assert!(!grants_composio_explicit(&["composiotools".into()]));
    }

    /// The `[tools.composio]` sub-section parses its toolkit allowlist and an
    /// absent section defaults to open mode (empty list).
    #[test]
    fn tools_composio_section_parses_toolkits_and_defaults_empty() {
        let with_section: CompanyManifest = toml::from_str(
            "[company]\nname = \"Acme\"\n[tools.composio]\ntoolkits = [\"gmail\", \"slack\"]\n",
        )
        .unwrap();
        assert_eq!(
            with_section.tools.composio.toolkits,
            vec!["gmail".to_string(), "slack".to_string()]
        );
        let without: CompanyManifest = toml::from_str("[company]\nname = \"Acme\"\n").unwrap();
        assert!(without.tools.composio.toolkits.is_empty());
    }

    // Guards the newly-added `Serialize` derive: a manifest with renamed
    // `[[agent]]`/`[[schedule]]` arrays must survive a serialize→deserialize
    // round-trip through JSON without dropping the renamed fields.
    #[test]
    fn manifest_serialize_deserialize_round_trips() {
        let toml_src = r#"
            [company]
            name = "Acme"
            output = "widgets"

            [[agent]]
            id = "ceo"
            role = "Chief"
            tools = ["email.send"]

            [[schedule]]
            cron = "0 9 * * *"
            prompt = "daily standup"

            [policy]
            mode = "supervised"
            auto_approve_under_usd = 5.0
        "#;
        let manifest: CompanyManifest = toml::from_str(toml_src).expect("parse toml");

        let json = serde_json::to_string(&manifest).expect("serialize");
        let back: CompanyManifest = serde_json::from_str(&json).expect("deserialize");

        assert_eq!(back.company.name, "Acme");
        assert_eq!(back.agents.len(), 1);
        assert_eq!(back.agents[0].id, "ceo");
        assert_eq!(back.schedules.len(), 1);
        assert_eq!(back.schedules[0].cron, "0 9 * * *");
        assert_eq!(back.policy.auto_approve_under_usd, Some(5.0));

        // The renamed arrays serialize under their manifest keys.
        let value = serde_json::to_value(&manifest).unwrap();
        assert!(value.get("agent").is_some());
        assert!(value.get("schedule").is_some());
    }

    /// `Agent.context` is `Option<Vec<String>>`, not a defaulted `Vec`,
    /// specifically so an omitted `context` key and an explicit `context = []`
    /// stay distinguishable (docs/spec/runtime/orchestration/alignment.md's
    /// per-tier-default rule depends on this). Pin the manifest round-trip for
    /// both spellings so a regression to a defaulted `Vec` — which would
    /// collapse them back to the same value — fails a test instead of shipping
    /// silently.
    #[test]
    fn agent_context_distinguishes_omitted_from_explicit_empty() {
        let omitted: Agent = toml::from_str(
            r#"
            id = "critic"
            role = "Critic"
            "#,
        )
        .expect("parse toml");
        assert_eq!(
            omitted.context, None,
            "an omitted `context` key MUST deserialize to None, not an empty vec"
        );

        let explicit_empty: Agent = toml::from_str(
            r#"
            id = "critic"
            role = "Critic"
            context = []
            "#,
        )
        .expect("parse toml");
        assert_eq!(
            explicit_empty.context,
            Some(vec![]),
            "an explicit `context = []` MUST deserialize to Some(vec![]), distinct from None"
        );

        let populated: Agent = toml::from_str(
            r#"
            id = "critic"
            role = "Critic"
            context = ["GOAL.md", "claims.md"]
            "#,
        )
        .expect("parse toml");
        assert_eq!(
            populated.context,
            Some(vec![
                ContextEntry::from("GOAL.md"),
                ContextEntry::from("claims.md")
            ])
        );

        // The distinction survives a JSON round-trip too, since the routing
        // layer this field feeds may cross that boundary (e.g. the console).
        let json = serde_json::to_string(&omitted).expect("serialize");
        let back: Agent = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.context, None);
    }

    /// A bare `context` string is `Read`; `{ path, access = "write" }` is the
    /// only way to grant `Write`. `write_scope` collects exactly the write
    /// entries, and `None` — either an omitted `context` key or a `context`
    /// with no write entry — is unconfined, not "confined to nothing".
    #[test]
    fn write_scope_is_none_unless_a_context_entry_declares_write() {
        let omitted: Agent = toml::from_str("id = \"critic\"\nrole = \"Critic\"\n").unwrap();
        assert_eq!(
            omitted.write_scope(),
            None,
            "an omitted context key is unconfined"
        );

        let read_only: Agent =
            toml::from_str("id = \"critic\"\nrole = \"Critic\"\ncontext = [\"brand/Voice.md\"]\n")
                .unwrap();
        assert_eq!(
            read_only.write_scope(),
            None,
            "a read-only context list is unconfined, not confined to nothing"
        );

        let write_entry: Agent = toml::from_str(
            r#"
            id = "critic"
            role = "Critic"
            context = ["brand/Voice.md", { path = "agents/critic/notes.md", access = "write" }]
            "#,
        )
        .expect("parse toml");
        assert_eq!(
            write_entry.write_scope(),
            Some(vec!["agents/critic/notes.md".to_string()]),
            "only the declared write entry is in scope, not the read one"
        );
    }

    /// An omitted `ledgers` key is unrestricted `Record` access to every slug
    /// — the tool surface every agent had before this field existed. A
    /// declared list answers only for the slugs it names.
    #[test]
    fn ledger_access_defaults_to_unrestricted_record() {
        let unrestricted: Agent = toml::from_str("id = \"critic\"\nrole = \"Critic\"\n").unwrap();
        assert_eq!(
            unrestricted.ledger_access("tasks"),
            Some(LedgerAccess::Record)
        );
        assert_eq!(
            unrestricted.ledger_access("anything"),
            Some(LedgerAccess::Record)
        );

        let scoped: Agent = toml::from_str(
            r#"
            id = "critic"
            role = "Critic"
            ledgers = [
                { name = "tasks", access = "record" },
                { name = "decisions", access = "read" },
            ]
            "#,
        )
        .unwrap();
        assert_eq!(scoped.ledger_access("tasks"), Some(LedgerAccess::Record));
        assert_eq!(scoped.ledger_access("DECISIONS"), Some(LedgerAccess::Read));
        assert_eq!(
            scoped.ledger_access("goals"),
            None,
            "an undeclared slug is unreachable"
        );
    }

    /// A bare `{ name = "tasks" }` grant, with no `access` key, defaults to
    /// `Read` — the safer of the two, so declaring a `ledgers` list without
    /// stating an access level does not silently hand out write access.
    #[test]
    fn a_ledger_grant_with_no_access_key_defaults_to_read() {
        let agent: Agent = toml::from_str(
            "id = \"critic\"\nrole = \"Critic\"\nledgers = [{ name = \"tasks\" }]\n",
        )
        .unwrap();
        assert_eq!(agent.ledger_access("tasks"), Some(LedgerAccess::Read));
    }

    /// The `[plan]` section (issue #108) survives a TOML → struct → JSON → struct
    /// round-trip, and an absent section deserializes to the not-set default.
    #[test]
    fn plan_section_round_trips_and_defaults() {
        let toml_src = r#"
            [company]
            name = "Acme"

            [plan]
            name = "starter"
            period = "monthly"
            total_tokens = 2000000

            [plan.token_budgets]
            web = 500000
        "#;
        let manifest: CompanyManifest = toml::from_str(toml_src).expect("parse toml");
        assert!(manifest.plan.is_set());
        assert_eq!(manifest.plan.name.as_deref(), Some("starter"));
        assert_eq!(manifest.plan.period, "monthly");
        assert_eq!(manifest.plan.token_budgets.get("web"), Some(&500_000));
        assert_eq!(manifest.plan.total_tokens, Some(2_000_000));

        let json = serde_json::to_string(&manifest).expect("serialize");
        let back: CompanyManifest = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.plan.name.as_deref(), Some("starter"));
        assert_eq!(back.plan.period, "monthly");
        assert_eq!(back.plan.token_budgets.get("web"), Some(&500_000));
        assert_eq!(back.plan.total_tokens, Some(2_000_000));

        // An absent `[plan]` defaults to period-only, which is NOT set.
        let bare: CompanyManifest = toml::from_str("[company]\nname = \"Bare\"\n").unwrap();
        assert!(!bare.plan.is_set());
        assert_eq!(bare.plan.period, "daily");
        assert_eq!(bare.plan.total_tokens, None);
    }

    /// A `[plan]` carrying **only** a `total_tokens` ceiling (issue #188) — no
    /// name, no per-namespace budgets — is still a set plan, so the total gate
    /// engages on its own.
    #[test]
    fn plan_total_tokens_only_is_set() {
        let manifest: CompanyManifest =
            toml::from_str("[company]\nname = \"Acme\"\n[plan]\ntotal_tokens = 1000\n").unwrap();
        assert!(manifest.plan.is_set(), "a total-only plan must be set");
        assert_eq!(manifest.plan.total_tokens, Some(1000));
        assert!(manifest.plan.name.is_none());
        assert!(manifest.plan.token_budgets.is_empty());
    }

    /// Helper: the grant list shape every predicate here takes.
    fn grants(list: &[&str]) -> Vec<String> {
        list.iter().map(|g| g.to_string()).collect()
    }

    /// **The asymmetry, pinned.** A bare `*` confers publishing and does NOT
    /// confer `repo`.
    ///
    /// This test exists to stop a future tidy-up, not to describe a subtlety.
    /// `grants_files_or_docs` sits in a file of `grants_*_explicit` siblings
    /// that all reject `*`, and folding it into that family is the obvious
    /// "consistency" edit. It would be a silent revocation: most shipped
    /// manifests grant `*` and nothing else, so publishing would switch off for
    /// them with no error anywhere — agents that can still write files and can
    /// no longer deliver one.
    #[test]
    fn a_bare_wildcard_confers_publishing() {
        let wildcard = grants(&["*"]);
        assert!(
            grants_files_or_docs(&wildcard),
            "a bare `*` must confer publishing — it is what most shipped manifests grant"
        );
        // The ordinary namespace forms confer it too.
        for grant in ["files", "docs", "files.write", "docs.read"] {
            assert!(
                grants_files_or_docs(&grants(&[grant])),
                "`{grant}` must confer publishing"
            );
        }
    }

    /// The boundary rule, pinned against a naive `starts_with`.
    ///
    /// A documentation-flavoured grant is not a grant on `docs`, and a
    /// filesystem-flavoured one is not a grant on `files`. `docsy` and
    /// `filesystem` are the cases a bare prefix test actually gets wrong: both
    /// extend the namespace without stopping on a separator, so `starts_with`
    /// accepts them and would hand `publish_artifact` (and, through the shared
    /// `wants_files` gate, the whole file belt) to an agent the manifest never
    /// granted it to. Issue #461 removed this class of disagreement by routing
    /// every grant match through `extends_on_boundary`; this asserts the
    /// publishing predicate is on that side of it.
    #[test]
    fn documentation_grant_does_not_confer_publishing() {
        for grant in [
            "documentation",
            "documentation.read",
            "docsy",
            "filesystem",
            "filesystem.wipe",
            "web",
            "shell",
        ] {
            assert!(
                !grants_files_or_docs(&grants(&[grant])),
                "`{grant}` is not a grant on the files/docs namespace"
            );
        }

        // …and the real `e2e_harness` allow list, which grants no file family,
        // confers nothing either.
        assert!(
            !grants_files_or_docs(&grants(&[
                "composio",
                "mcp:*",
                "workspace",
                "workspace.*",
                "web"
            ])),
            "the shipped e2e_harness grants confer no publishing"
        );
    }
}
