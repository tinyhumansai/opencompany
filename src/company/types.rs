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

/// Inference providers selectable in `[inference].provider` (issue #56 — BYOK).
///
/// * `managed` — the hosted TinyHumans / Medulla brain (the default path).
/// * `openrouter` — OpenRouter's OpenAI-compatible aggregator (needs a key +
///   the `HTTP-Referer` / `X-Title` attribution headers).
/// * `openai_compatible` — any OpenAI-compatible endpoint the tenant runs
///   (needs a `base_url`, usually a key).
/// * `ollama` — a local Ollama server's OpenAI-compatible surface (needs a
///   `base_url`; no key).
pub const INFERENCE_PROVIDERS: &[&str] = &["managed", "openrouter", "openai_compatible", "ollama"];

/// The abstract cognition tiers the tenant `[inference].models` table maps to
/// concrete provider model ids. These are the workload names the harness
/// addresses; an unmapped tier passes through to the provider verbatim.
pub const INFERENCE_TIERS: &[&str] = &["chat-v1", "reasoning-v1", "agentic-v1", "vision-v1"];

/// Tool providers selectable in `[tools].provider`.
pub const TOOL_PROVIDERS: &[&str] = &["openhuman", "builtin"];

/// Approval policy modes selectable in `[policy].mode`, mirroring OpenHuman's
/// security tiers.
pub const POLICY_MODES: &[&str] = &["readonly", "supervised", "full"];

/// Channels the runtime knows how to enable under `[channels.*]`.
pub const KNOWN_CHANNELS: &[&str] = &["operator", "email", "slack", "sms", "web", "telegram"];

/// Effect kinds gated for approval by default under a `supervised` policy.
pub const DEFAULT_ALWAYS_APPROVE: &[&str] = &["payment.send", "filing.submit", "external.publish"];

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
}

/// A `[[agent]]` roster entry.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Agent {
    /// snake_case, unique within the roster.
    pub id: String,
    /// Human-readable job title.
    pub role: String,
    /// What this agent does.
    #[serde(default)]
    pub description: Option<String>,
    /// Cognition tier hint; never selects a model.
    #[serde(default)]
    pub tier: Option<String>,
    /// Tool grant globs, intersected with `[tools].allow`.
    #[serde(default)]
    pub tools: Vec<String>,
    /// Per-agent daily spend cap in USD.
    #[serde(default)]
    pub budget_usd_daily: Option<f64>,
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
    30
}

fn default_true() -> bool {
    true
}

/// `[workflows]` — references to the workflow graphs to enable. The graphs live
/// as separate files under the company's `workflows/` directory.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Workflows {
    /// Workflow ids to enable, each a `workflows/<id>.toml` graph file.
    #[serde(default)]
    pub enabled: Vec<String>,
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
/// admins = ["ada@example.com"]
/// ```
///
/// Listing an address does not create an account. It makes that address
/// *eligible* to log in, at which point redeeming a magic link mints the user
/// as an admin. Removing an address from the manifest stops it bootstrapping
/// again but does not delete an account it already created — use the admin
/// routes for that.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Users {
    /// Email addresses that may log in as admins without being invited first.
    #[serde(default)]
    pub admins: Vec<String>,
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
#[derive(Clone, Debug, Serialize, Deserialize)]
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
}

/// Daily `web_search` call ceiling applied when `[tools].search_daily_calls` is
/// absent (issue #238).
///
/// Sized as a working day of research, not a hard product limit: at the managed
/// backend's ~$0.01/request list price this bounds an unattended runaway to
/// roughly $2/day/company while leaving a genuine multi-topic research session
/// (a handful of searches per question) comfortably inside it.
pub const DEFAULT_SEARCH_DAILY_CALLS: u32 = 200;

/// `[tools.composio]` — the per-tenant Composio toolkit allowlist (issue #110).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
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
            // code/web/subagent, and `media`/`composio` are listed literally
            // because the `*` wildcard deliberately excludes those two
            // (real-money + per-tenant-credential) namespaces. A company that
            // wants a narrower belt overrides `[tools].allow` explicitly.
            //
            // `search` (issue #238) is deliberately NOT in this list, unlike
            // `media`/`composio`: the #188 sign-off admitted it **opt-in**, so a
            // company that never asked for web search never spends on it.
            // Making it default-on is a one-word change here.
            allow: vec!["*".into(), "media".into(), "composio".into()],
            web_allowed_domains: Vec::new(),
            composio: ComposioTools::default(),
            search_daily_calls: None,
        }
    }
}

fn default_tool_provider() -> String {
    "openhuman".to_string()
}

/// `[policy]` — the default `ApprovalGate` configuration.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Policy {
    /// `readonly` | `supervised` (default) | `full`.
    #[serde(default = "default_policy_mode")]
    pub mode: String,
    /// Effect kinds that always park for approval regardless of amount.
    #[serde(default = "default_always_approve")]
    pub always_approve: Vec<String>,
    /// Spends strictly under this many USD skip approval.
    #[serde(default)]
    pub auto_approve_under_usd: Option<f64>,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            mode: default_policy_mode(),
            always_approve: default_always_approve(),
            auto_approve_under_usd: None,
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
}
