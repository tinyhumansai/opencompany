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

/// Tool providers selectable in `[tools].provider`.
pub const TOOL_PROVIDERS: &[&str] = &["openhuman", "builtin"];

/// Approval policy modes selectable in `[policy].mode`, mirroring OpenHuman's
/// security tiers.
pub const POLICY_MODES: &[&str] = &["readonly", "supervised", "full"];

/// Channels the runtime knows how to enable under `[channels.*]`.
pub const KNOWN_CHANNELS: &[&str] = &["operator", "email", "slack", "sms", "web"];

/// Effect kinds gated for approval by default under a `supervised` policy.
pub const DEFAULT_ALWAYS_APPROVE: &[&str] = &["payment.send", "filing.submit", "external.publish"];

/// Priorities a company may assign to a prioritized `[[connection]]`.
pub const CONNECTION_PRIORITIES: &[&str] = &["low", "medium", "high"];

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
}

impl Default for Tools {
    fn default() -> Self {
        Self {
            provider: default_tool_provider(),
            allow: Vec::new(),
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
}
