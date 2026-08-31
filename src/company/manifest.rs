//! Manifest loading, discovery, and validation.
//!
//! [`CompanyManifest::from_path`] parses a manifest file and validates it,
//! returning every problem at once in prosumer language. [`discover`] locates
//! the manifest inside a company directory, preferring `company.toml` over the
//! legacy `agents.toml`.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use crate::error::{OpenCompanyError, Result};
use crate::ports::decode_wallet_address;

use super::types::{
    ACP_AGENTS, ACP_TRANSPORTS, AUTH_MODES, BRAIN_MODES, CONNECTION_PRIORITIES, CompanyManifest,
    GATEABLE_NAMESPACES, HARNESS_KINDS, Harness, IMPLICIT_HARNESS_ID, Inference, KNOWN_CHANNELS,
    MAX_DELEGATION_DEPTH_BOUNDS, PLAN_NAMES, PLAN_PERIODS, POLICY_MODES, PROMPT_CLASSES, TIERS,
    TOOL_PROVIDERS,
};

/// The `delegates_to` entry that means "every desk this company has".
///
/// Shared with the runtime allowlist check
/// ([`reject_out_of_allowlist_target`](crate::runtime::delegation_tools::reject_out_of_allowlist_target))
/// so validation and enforcement cannot disagree about what `"*"` means.
pub const DELEGATES_TO_WILDCARD: &str = "*";

/// Preferred manifest filename.
pub const MANIFEST_FILE: &str = "company.toml";

/// Legacy manifest filename, accepted unchanged with a deprecation note.
pub const LEGACY_MANIFEST_FILE: &str = "agents.toml";

/// A located manifest file and whether it uses the legacy filename.
#[derive(Clone, Debug)]
pub struct Located {
    /// Path to the manifest file.
    pub path: PathBuf,
    /// True when the file is the legacy `agents.toml`.
    pub legacy: bool,
}

/// Locates the manifest inside a directory (or accepts a direct file path),
/// preferring `company.toml` over `agents.toml`.
pub fn discover(input: &Path) -> Result<Located> {
    if input.is_file() {
        let legacy = input.file_name().and_then(|n| n.to_str()) == Some(LEGACY_MANIFEST_FILE);
        return Ok(Located {
            path: input.to_path_buf(),
            legacy,
        });
    }

    let preferred = input.join(MANIFEST_FILE);
    if preferred.is_file() {
        return Ok(Located {
            path: preferred,
            legacy: false,
        });
    }

    let legacy = input.join(LEGACY_MANIFEST_FILE);
    if legacy.is_file() {
        return Ok(Located {
            path: legacy,
            legacy: true,
        });
    }

    Err(OpenCompanyError::MissingManifest(input.to_path_buf()))
}

impl CompanyManifest {
    /// The company's harnesses, with the implicit one synthesized when the
    /// manifest declares none.
    ///
    /// **Read harnesses through this, never through the `harnesses` field.** A
    /// company with no `[[harness]]` block still runs on a harness — the
    /// `built_in` one on the company-level `[inference]` — and a caller that
    /// looked at the bare field would see an empty list and conclude the company
    /// has no engine, which is never true.
    pub fn effective_harnesses(&self) -> Vec<Harness> {
        if self.harnesses.is_empty() {
            return vec![Harness::implicit()];
        }
        self.harnesses.clone()
    }

    /// The id of the harness agents naming none run on.
    ///
    /// The entry marked `default = true`; the first declared if validation was
    /// skipped and none is marked, so this is total rather than panicking on a
    /// manifest that reached here unvalidated.
    pub fn default_harness_id(&self) -> String {
        let harnesses = self.effective_harnesses();
        harnesses
            .iter()
            .find(|h| h.default)
            .or_else(|| harnesses.first())
            .map(|h| h.id.clone())
            .unwrap_or_else(|| IMPLICIT_HARNESS_ID.to_string())
    }

    /// The default harness's `[harness.inference]`, when that harness declares
    /// one; `None` when it runs on the company-level `[inference]`.
    ///
    /// The default harness is the one the base provider resolves for, and its
    /// own inference section must beat the company-level one — the same
    /// precedence a named harness gets in [`lanes::build`](crate::harness::lanes::build).
    /// `None` (not "empty") because an absent declaration means "fall back to
    /// `[inference]`", which the caller already holds.
    pub fn default_harness_inference(&self) -> Option<Inference> {
        let default_id = self.default_harness_id();
        self.effective_harnesses()
            .into_iter()
            .find(|h| h.id == default_id)
            .and_then(|h| h.inference)
    }

    /// The full default `Harness`, resolved by [`default_harness_id`](Self::default_harness_id).
    ///
    /// Total, like `default_harness_id` — falls back to the implicit `built_in`
    /// harness so this never panics on a manifest reached before validation.
    /// Exists so callers that need more than the id (chiefly `kind`, to decide
    /// whether the default lane is even runnable — see
    /// [`lanes::build`](crate::harness::lanes::build)) do not each re-derive
    /// the same "find by default id" lookup [`default_harness_inference`](Self::default_harness_inference)
    /// already does.
    pub fn default_harness(&self) -> Harness {
        let default_id = self.default_harness_id();
        self.effective_harnesses()
            .into_iter()
            .find(|h| h.id == default_id)
            .unwrap_or_else(Harness::implicit)
    }

    /// The harness `agent_id` runs on, resolving an unset binding to the
    /// default. `None` only when the named harness does not exist — which
    /// [`validate`](Self::validate) rejects, so a validated manifest always
    /// answers.
    ///
    /// An id that no `[[harness]]` declares but that names a coding CLI this
    /// build can drive locally resolves to
    /// [`Harness::implicit_local`](crate::company::Harness::implicit_local) —
    /// see that constructor for why a local ACP harness is not something a
    /// `company.toml` should have to declare. A declared harness of the same
    /// id is found first and always wins.
    pub fn harness_for(&self, agent_id: &str) -> Option<Harness> {
        let named = self
            .agents
            .iter()
            .find(|a| a.id == agent_id)
            .and_then(|a| a.harness.clone());
        let want = named.unwrap_or_else(|| self.default_harness_id());
        self.harness_by_id(&want)
    }

    /// The harness `id` names: a declared `[[harness]]` first, else the
    /// synthesized local one when `id` is a coding CLI this build drives.
    ///
    /// The single resolver for "does this harness id mean anything?" — used
    /// by [`harness_for`](Self::harness_for) resolving an agent's own binding
    /// and by the console's write path validating a submitted one. Two copies
    /// of the declared-then-implicit precedence would eventually disagree,
    /// and the shape that disagreement takes is a harness the picker offers
    /// and the `PATCH` then refuses.
    pub fn harness_by_id(&self, id: &str) -> Option<Harness> {
        self.effective_harnesses()
            .into_iter()
            .find(|h| h.id == id)
            .or_else(|| Harness::is_implicit_local_id(id).then(|| Harness::implicit_local(id)))
    }

    /// Reads, parses, and validates a manifest from `path`.
    ///
    /// `path` may be a manifest file or a directory containing one. Validation
    /// collects every problem and reports them together.
    ///
    /// When `path` resolves to a bundle carrying an `agents/` directory, the
    /// roster is read from those per-teammate files instead of from
    /// `[[agent]]` — see [`agent_file`](super::agent_file).
    pub fn from_path(path: impl AsRef<Path>) -> Result<Self> {
        Self::from_located(&discover(path.as_ref())?)
    }

    /// [`from_path`](Self::from_path), but does not fail a
    /// [`RESERVED_AGENT_IDS`](crate::ports::types::RESERVED_AGENT_IDS)
    /// agent-id collision — see [`validate_with`](Self::validate_with).
    ///
    /// `register_company`'s `serve` boot loop is this method's caller: every
    /// hosted tenant's `company.toml` is the durable record of that company
    /// (`companies/<name>`, loaded fresh on each container restart per
    /// `CLAUDE.md`'s "Running under the platform harness"), not a one-time
    /// authoring artifact. [`from_path`](Self::from_path) — used by
    /// `opencompany check` and fresh provisioning — stays strict on purpose:
    /// those *are* the authoring flow, and should refuse an id someone just
    /// typed. This method is for the reload that must not refuse an id that
    /// was fine when the company started (issue #1781 review, Codex P1).
    pub fn from_path_for_reload(path: impl AsRef<Path>) -> Result<Self> {
        Self::from_located_with(&discover(path.as_ref())?, false)
    }

    /// Loads an already-[`discover`]ed manifest, folding in what the bundle
    /// around it declares: the roster from `agents/*.toml` when it has one, and
    /// the MCP servers from `mcp.json`.
    ///
    /// This — not [`from_file`](Self::from_file) — is what every production
    /// caller reaches through, which is why the bundle merge lives here.
    ///
    /// Split out from [`from_path`](Self::from_path) so callers that need the
    /// [`Located`] value for themselves — `opencompany check`, which prints the
    /// legacy-filename deprecation note — do not have to re-derive "is this a
    /// bundle roster?" on their own. That duplication is not hypothetical: the
    /// check command called [`from_file`](Self::from_file) directly and silently
    /// reported every desk member as "not an agent in the roster", because it
    /// had validated a manifest whose roster it had never loaded.
    pub(crate) fn from_located(located: &Located) -> Result<Self> {
        Self::from_located_with(located, true)
    }

    /// [`from_located`](Self::from_located), with
    /// [`RESERVED_AGENT_IDS`](crate::ports::types::RESERVED_AGENT_IDS)
    /// enforcement toggled — see [`from_path_for_reload`](Self::from_path_for_reload).
    fn from_located_with(located: &Located, enforce_reserved_agent_ids: bool) -> Result<Self> {
        // The bundle root is the located manifest's own parent, whether the
        // caller passed the directory or the file itself: `discover` accepts
        // both, and deriving the root from the located manifest is what keeps
        // the two call forms from resolving `agents/` differently.
        match located.path.parent() {
            Some(bundle) => {
                Self::from_file_in_bundle(&located.path, bundle, enforce_reserved_agent_ids)
            }
            None => Self::from_file_with(&located.path, enforce_reserved_agent_ids),
        }
    }

    /// [`from_file`](Self::from_file), with everything the bundle around the
    /// manifest declares folded in: the roster from `agents/*.toml` and the MCP
    /// servers from `mcp.json`.
    ///
    /// Both are merged **before** validation, so a bundle-declared server is
    /// held to exactly the rules an inline `[[mcp_server]]` is — the HTTP-only
    /// transport boundary, the credential-free endpoint, the unique name —
    /// without a second copy of them living in the parser.
    fn from_file_in_bundle(
        path: &Path,
        bundle: &Path,
        enforce_reserved_agent_ids: bool,
    ) -> Result<Self> {
        let mut manifest = Self::parse_file(path)?;

        if super::agent_file::has_agent_files(bundle) {
            if !manifest.agents.is_empty() {
                return Err(OpenCompanyError::ManifestInvalid {
                    path: path.to_path_buf(),
                    problems: vec![format!(
                        "this company defines its roster in `{dir}/*.toml`, but `{file}` also has `[[agent]]` entries — the two forms are exclusive, so remove the `[[agent]]` blocks or delete the `{dir}/` directory.",
                        dir = super::agent_file::AGENTS_DIR,
                        file = path
                            .file_name()
                            .and_then(|name| name.to_str())
                            .unwrap_or(MANIFEST_FILE),
                    )],
                });
            }
            manifest.agents = super::agent_file::load_agents(bundle)?;
        }

        let problems = manifest.merge_bundle_mcp_servers(bundle, path);
        manifest.into_validated_with(path, problems, enforce_reserved_agent_ids)
    }

    /// Folds `<bundle>/mcp.json` into `mcp_servers`, returning every problem the
    /// file carried.
    ///
    /// A name declared in both `mcp.json` and an inline `[[mcp_server]]` is
    /// refused rather than resolved by precedence — the rule the roster already
    /// uses for the same situation, and for the same reason: either precedence
    /// rule silently discards a declaration somebody wrote down, and a server
    /// that quietly is not the one you configured is worse than one that
    /// refuses to start.
    fn merge_bundle_mcp_servers(&mut self, bundle: &Path, path: &Path) -> Vec<String> {
        let (servers, mut problems) = super::mcp_file::load_dir_mcp_servers(bundle);
        for server in servers {
            if self
                .mcp_servers
                .iter()
                .any(|existing| existing.name.trim() == server.name)
            {
                problems.push(format!(
                    "mcp server `{}` is declared in both `{}` and `{}` — the two forms are \
                     exclusive per server, so keep one.",
                    server.name,
                    super::mcp_file::MCP_FILE,
                    path.file_name()
                        .and_then(|name| name.to_str())
                        .unwrap_or(MANIFEST_FILE),
                ));
                continue;
            }
            self.mcp_servers.push(server);
        }
        problems
    }

    /// Reads, parses, and validates a specific manifest file.
    pub fn from_file(path: &Path) -> Result<Self> {
        Self::from_file_with(path, true)
    }

    /// [`from_file`](Self::from_file), with
    /// [`RESERVED_AGENT_IDS`](crate::ports::types::RESERVED_AGENT_IDS)
    /// enforcement toggled — see [`from_path_for_reload`](Self::from_path_for_reload).
    fn from_file_with(path: &Path, enforce_reserved_agent_ids: bool) -> Result<Self> {
        Self::parse_file(path)?.into_validated(path, enforce_reserved_agent_ids)
    }

    /// Reads and deserializes a manifest file, without validating it.
    fn parse_file(path: &Path) -> Result<Self> {
        let text =
            std::fs::read_to_string(path).map_err(|source| OpenCompanyError::ManifestRead {
                path: path.to_path_buf(),
                source,
            })?;

        toml::from_str(&text).map_err(|err| {
            OpenCompanyError::ManifestParse(path.to_path_buf(), err.message().to_string())
        })
    }

    /// Parses a manifest that came back out of the store, applying the global
    /// baseline to it.
    ///
    /// **The read path every store backend uses**, rather than a bare
    /// `toml::from_str`, because a company is provisioned once and read
    /// thereafter: a baseline applied only where bundles are parsed would reach
    /// new companies and no existing one. [`apply_globals`](Self::apply_globals)
    /// is idempotent, so re-applying it on every load is what makes a baseline
    /// change — or a newly written `[globals].disable` — take effect on the next
    /// read instead of at the next reprovision.
    ///
    /// Deliberately does **not** validate: a stored manifest was validated when
    /// it was accepted, and failing a load over a rule that tightened since
    /// would strand a company that is already running.
    pub fn from_stored_toml(toml_src: &str) -> std::result::Result<Self, toml::de::Error> {
        let mut manifest: Self = toml::from_str(toml_src)?;
        manifest.apply_globals();
        Ok(manifest)
    }

    /// Merges the global baseline ([`crate::globals`]) into this manifest's
    /// roster.
    ///
    /// Idempotent, and safe to call on a manifest that already carries merged
    /// globals: every teammate marked [`Agent::global`] is dropped first, then
    /// the current baseline is re-appended. That is what lets a stored manifest
    /// — which is serialized back out with the merged roster in it — pick up a
    /// changed baseline and honour a `[globals].disable` entry written later.
    ///
    /// Two ordering rules, both protecting the same thing:
    ///
    /// * globals are appended **after** the company's own roster, because
    ///   [`orchestrator_id`](super::orchestrator_id) falls back to the first
    ///   agent declared when nobody is tagged — prepending would hand a company
    ///   with an untagged roster to a global teammate;
    /// * an id the company already declares is **skipped**, so a company's own
    ///   `researcher` supersedes the global one outright rather than merging
    ///   with it field by field.
    pub fn apply_globals(&mut self) {
        self.agents.retain(|agent| !agent.global);
        for global in crate::globals::agents() {
            if crate::globals::disabled(&self.globals.disable, "agent", &global.id) {
                continue;
            }
            if self.agents.iter().any(|agent| agent.id == global.id) {
                continue;
            }
            let mut agent = global.clone();
            agent.global = true;
            self.agents.push(agent);
        }
    }

    /// Runs [`validate`](Self::validate), reporting every problem against `path`.
    ///
    /// The baseline is merged **after** validation, not before: a global is
    /// already parsed and checked by [`crate::globals`], and running it through
    /// this validator would let one malformed global fail every company on the
    /// host rather than only itself.
    fn into_validated(self, path: &Path, enforce_reserved_agent_ids: bool) -> Result<Self> {
        self.into_validated_with(path, Vec::new(), enforce_reserved_agent_ids)
    }

    /// [`into_validated`](Self::into_validated), carrying problems the caller
    /// already found.
    ///
    /// Bundle files that are not the manifest — `mcp.json` today — are parsed
    /// before validation runs, and what they found has to reach the same
    /// refusal. Reported first, because a file that would not parse is the
    /// thing to fix before anything the manifest says about it.
    fn into_validated_with(
        mut self,
        path: &Path,
        mut problems: Vec<String>,
        enforce_reserved_agent_ids: bool,
    ) -> Result<Self> {
        problems.extend(self.validate_with(enforce_reserved_agent_ids));
        if problems.is_empty() {
            self.apply_globals();
            Ok(self)
        } else {
            Err(OpenCompanyError::ManifestInvalid {
                path: path.to_path_buf(),
                problems,
            })
        }
    }

    /// Returns every validation problem in prosumer language. An empty vector
    /// means the manifest is valid.
    pub fn validate(&self) -> Vec<String> {
        self.validate_with(true)
    }

    /// The subset of [`validate`](Self::validate) that exists *only* because
    /// of the [`RESERVED_AGENT_IDS`](crate::ports::types::RESERVED_AGENT_IDS)/
    /// `operator` reservation — `validate_with(true)` minus
    /// `validate_with(false)`.
    ///
    /// `RuntimeBuilder::build` (issue #1781 review, Codex P1 follow-up) uses
    /// this to tell a reserved-id/name collision the *previously stored*
    /// manifest already carried — genuinely grandfathered, however old the
    /// company — from one an operator just introduced by editing
    /// `company.toml` between two `serve` restarts. `existing.is_some()`
    /// alone is not that test: it is true for every restart forever, so
    /// gating strict enforcement on it alone (the shape `b80c45e2c` shipped)
    /// let a post-first-boot edit mint `system`, `main`, `general`, or an
    /// `operator`-colliding desk on every subsequent reboot, impersonating a
    /// built-in surface. Diffing against the stored record's own
    /// `reserved_problems()` keeps the grandfather narrow: a collision must
    /// already have been present in what this store last saved, not merely
    /// possible to explain away as "some restart, sometime."
    pub(crate) fn reserved_problems(&self) -> Vec<String> {
        let relaxed: std::collections::HashSet<String> =
            self.validate_with(false).into_iter().collect();
        self.validate_with(true)
            .into_iter()
            .filter(|problem| !relaxed.contains(problem))
            .collect()
    }

    /// [`validate`](Self::validate), with the [`RESERVED_AGENT_IDS`](crate::ports::types::RESERVED_AGENT_IDS)
    /// agent-id collision, and the matching `operator` group-chat id/name
    /// reservation, reported only when `enforce_reserved_agent_ids` is set.
    ///
    /// [`from_path_for_reload`](Self::from_path_for_reload) calls this with
    /// `false`: that rule shipped after companies already existed whose
    /// roster declared an agent at one of those ids — or whose desk list
    /// declared a group chat at the `operator` id or name (`operator`,
    /// chiefly — see the grandfather-support machinery in `channel.rs`,
    /// `operator.rs`, `delivery.rs`, and `runtime.rs`, all built to run
    /// exactly this manifest shape correctly), and this method's boot-time
    /// caller reloads that same on-disk manifest on every restart, not just
    /// once at authoring time. Every other problem below is still reported
    /// either way — this grandfathers the reserved-`operator`-identity rules
    /// proven to predate existing manifests (agent id, plus group-chat id and
    /// name), not validation as a whole (issue #1781 review, Codex P1: the
    /// group-chat arm was still unconditional after the agent-id arm was
    /// gated, so a company whose desk list predates the reservation could
    /// still fail to reboot).
    fn validate_with(&self, enforce_reserved_agent_ids: bool) -> Vec<String> {
        let mut problems = Vec::new();

        if self.company.name.trim().is_empty() {
            problems.push("`[company].name` cannot be empty — give your company a name.".into());
        }

        // Roster: ids must be snake_case and unique; tiers and budgets sane.
        let mut seen = std::collections::HashSet::new();
        for (index, agent) in self.agents.iter().enumerate() {
            let label = if agent.id.is_empty() {
                format!("agent #{}", index + 1)
            } else {
                format!("agent `{}`", agent.id)
            };

            if agent.id.trim().is_empty() {
                problems.push(format!("{label} is missing an `id`."));
            } else if !is_snake_case(&agent.id) {
                problems.push(format!(
                    "{label} has an invalid `id` — use snake_case (lowercase letters, digits, and underscores, starting with a letter)."
                ));
            } else if enforce_reserved_agent_ids
                && crate::ports::types::RESERVED_AGENT_IDS
                    .iter()
                    .any(|reserved| agent.id.eq_ignore_ascii_case(reserved))
            {
                // Issue #1757 follow-up: `RESERVED_AGENT_IDS` already stops a
                // console-minted teammate from taking one of these ids
                // (`CompanyRecord::mint_agent_id`), but a manifest agent's id
                // comes straight from the TOML and was never checked against
                // the same list — so `operator`, `agents`, `desks`, or
                // `system` could still be declared here and collide with the
                // built-in surface each one names (the desk list, the
                // workspace roots, or the runtime's own author id). The
                // `operator` case additionally has its own dedicated message
                // below (group chats), because a group chat and an agent
                // collide with it in different, more specific ways; this arm
                // covers the agent side for the whole reserved set.
                problems.push(format!(
                    "{label} uses the reserved id `{}`, which OpenCompany keeps for its own use — choose a different id.",
                    agent.id
                ));
            } else if !seen.insert(agent.id.as_str()) {
                problems.push(format!(
                    "agent `id` `{}` is used more than once — ids must be unique.",
                    agent.id
                ));
            }

            if agent.role.trim().is_empty() {
                problems.push(format!("{label} is missing a `role`."));
            }

            if let Some(tier) = &agent.tier
                && !TIERS.contains(&tier.as_str())
            {
                problems.push(one_of(&format!("{label} `tier`"), TIERS, tier));
            }

            if let Some(budget) = agent.budget_usd_daily
                && budget < 0.0
            {
                problems.push(format!(
                    "{label} `budget_usd_daily` cannot be negative — you wrote `{budget}`."
                ));
            }

            // Classes gate which routed documents this role may be told, so an
            // unrecognized entry is refused rather than ignored: a typo'd
            // exclusion is an exclusion that is not applied, and the whole point
            // of declaring the class explicitly is that it cannot be silently
            // lost. See `PROMPT_CLASSES`.
            for class in &agent.classes {
                if !PROMPT_CLASSES.contains(&class.as_str()) {
                    problems.push(one_of(
                        &format!("{label} `classes` entry"),
                        &PROMPT_CLASSES,
                        class,
                    ));
                }
            }
        }

        // Group chats: ids snake_case + unique; every member is a real agent.
        let mut chat_ids = std::collections::HashSet::new();
        for (index, chat) in self.group_chats.iter().enumerate() {
            let label = if chat.id.is_empty() {
                format!("group chat #{}", index + 1)
            } else {
                format!("group chat `{}`", chat.id)
            };

            if chat.id.trim().is_empty() {
                problems.push(format!("{label} is missing an `id`."));
            } else if !is_snake_case(&chat.id) {
                problems.push(format!(
                    "{label} has an invalid `id` — use snake_case (lowercase letters, digits, and underscores, starting with a letter)."
                ));
            } else if enforce_reserved_agent_ids
                && chat.id == crate::runtime::channel::OPERATOR_CHANNEL
            {
                // Issue #1757: `operator` is the reserved id of the built-in,
                // read-only Operator system channel — every company gets one,
                // listed and durable. A manifest desk claiming that id would be
                // indistinguishable from it in the desk list, and every message
                // sent there would be refused by the read-only guard in
                // `chat_and_emit` (`src/server/operator.rs`), which treats any
                // `chat_id == OPERATOR_CHANNEL` as the system feed regardless of
                // where it came from.
                //
                // Gated on `enforce_reserved_agent_ids` for the same reason the
                // agent-id reservation above is (issue #1781 review, Codex P1):
                // `operator` becoming reserved postdates real companies, and a
                // desk that already claimed the id — or the name, below — must
                // still reboot through `from_path_for_reload`, not just an
                // agent at the id. Authoring (`from_path`) stays strict.
                problems.push(format!(
                    "{label} uses the id `operator`, which is reserved for the built-in Operator channel — choose a different id."
                ));
            } else if enforce_reserved_agent_ids
                && chat
                    .name
                    .eq_ignore_ascii_case(crate::runtime::channel::OPERATOR_CHANNEL)
            {
                // Issue #1781 review (Codex P2): the id check above is not
                // enough on its own — `server::operator::resolve_desk`
                // matches a desk by id *or* case-insensitive name, so
                // `{id: "ops", name: "Operator"}` shadows the system channel
                // exactly as thoroughly as claiming the literal id would.
                // `GET {scope}/chat/history?desk=operator`, the request the
                // console's pinned read-only row makes, would resolve to this
                // desk instead of the system feed, and the desk's own
                // (writable, member) transcript would display through the
                // identity the console assumes is the read-only Operator
                // feed. Reserved for the same reason the id is — and gated
                // the same way (issue #1781 review, Codex P1 follow-up).
                problems.push(format!(
                    "{label} is named \"Operator\", which is reserved for the built-in Operator channel — choose a different name."
                ));
            } else if enforce_reserved_agent_ids
                && chat.name.eq_ignore_ascii_case(
                    crate::runtime::channel::OPERATOR_CHANNEL_COLLISION_FALLBACK,
                )
            {
                // Issue #1781 review (Codex/CodeRabbit P2 follow-up): the
                // name reservation above only blocks "Operator", but a
                // grandfathered collision diverts the durable feed to
                // `OPERATOR_CHANNEL_COLLISION_FALLBACK` ("operator-feed")
                // instead, and `server::operator::resolve_desk` folds a
                // `?desk=` selector against a desk's name exactly the same
                // way it folds it against "operator" — so a desk named
                // "operator-feed" would shadow the fallback feed precisely
                // as a desk named "Operator" would shadow the primary one.
                // Reserved for the same reason, gated the same way.
                problems.push(format!(
                    "{label} is named \"operator-feed\", which is reserved for the built-in Operator channel's fallback feed — choose a different name."
                ));
            } else if !chat_ids.insert(chat.id.as_str()) {
                problems.push(format!(
                    "group chat `id` `{}` is used more than once — ids must be unique.",
                    chat.id
                ));
            }

            if chat.name.trim().is_empty() {
                problems.push(format!("{label} is missing a `name`."));
            }

            for member in &chat.members {
                if !seen.contains(member.as_str()) {
                    problems.push(format!(
                        "{label} lists member `{member}`, which is not an agent in the roster."
                    ));
                }
            }
        }

        // Delegation allowlists (issue #176): every `delegates_to` entry must
        // name a desk this manifest actually declares.
        //
        // Checked here rather than in the roster loop above because it is the
        // one agent field whose target lives in a *later* section — the desks
        // are only fully known once `[[group_chat]]` has been walked. An entry
        // that resolves to nothing would otherwise fail silently at runtime:
        // the member would carry `delegate_to_desk`, every call would be
        // refused as off-allowlist, and the manifest would look fine.
        for agent in &self.agents {
            let label = if agent.id.is_empty() {
                "an agent".to_string()
            } else {
                format!("agent `{}`", agent.id)
            };
            for desk in &agent.delegates_to {
                let key = desk.trim();
                if key == DELEGATES_TO_WILDCARD {
                    continue;
                }
                if key.is_empty() {
                    problems.push(format!(
                        "{label} has an empty entry in `delegates_to` — list desk ids, or `\"*\"` for every desk."
                    ));
                    continue;
                }
                let resolves = self
                    .group_chats
                    .iter()
                    .any(|chat| chat.id == key || chat.name.eq_ignore_ascii_case(key));
                if !resolves {
                    problems.push(format!(
                        "{label} may delegate to `{key}`, which is not a desk in this company — `delegates_to` takes `[[group_chat]]` ids (or `\"*\"` for every desk), not teammate ids."
                    ));
                }
            }
        }

        // `ledgers` grants (per-agent ledger access): see `ledger_grant_problems`.
        let (builtin_ledgers, _) = crate::ledger::builtins();
        problems.extend(ledger_grant_problems(&self.agents, &builtin_ledgers));

        // Connections: a provider is required; a stated priority must be known.
        for (index, connection) in self.connections.iter().enumerate() {
            let label = if connection.provider.trim().is_empty() {
                format!("connection #{}", index + 1)
            } else {
                format!("connection `{}`", connection.provider)
            };

            if connection.provider.trim().is_empty() {
                problems.push(format!("{label} is missing a `provider`."));
            }

            if let Some(priority) = &connection.priority
                && !CONNECTION_PRIORITIES.contains(&priority.as_str())
            {
                problems.push(one_of(
                    &format!("{label} `priority`"),
                    CONNECTION_PRIORITIES,
                    priority,
                ));
            }
        }

        // MCP servers: unique names, an `http(s)://` endpoint, no stdio in v1.
        problems.extend(super::mcp::validate_servers(&self.mcp_servers));

        // Inference (issue #56 — BYOK): provider kind, base_url rules, and a
        // key *name* (never an inline credential). Inert when the section is
        // absent.
        problems.extend(super::inference::validate_inference(&self.inference));

        // Enabled workflows reference `workflows/<id>.toml`; ids must be sane.
        for id in &self.workflows.enabled {
            if !is_snake_case(id) {
                problems.push(format!(
                    "`[workflows].enabled` has an invalid workflow id `{id}` — use snake_case (a `workflows/{id}.toml` file)."
                ));
            }
        }

        // The concurrent-run ceiling must admit at least one run (issue #401). A
        // `0` is a misconfiguration that would refuse every run, so it fails
        // here rather than silently wedging the company's workflows.
        if self.workflows.max_in_flight_runs == 0 {
            problems.push(
                "`[workflows].max_in_flight_runs` must be at least 1 — a value of 0 would refuse every workflow run.".into(),
            );
        }

        if !BRAIN_MODES.contains(&self.brain.mode.as_str()) {
            problems.push(one_of("`[brain].mode`", BRAIN_MODES, &self.brain.mode));
        }

        problems.extend(self.validate_harnesses());

        problems.extend(self.validate_users());

        if !TOOL_PROVIDERS.contains(&self.tools.provider.as_str()) {
            problems.push(one_of(
                "`[tools].provider`",
                TOOL_PROVIDERS,
                &self.tools.provider,
            ));
        }

        // The delegation chain bound (issue #176). `0` would refuse the
        // orchestrator's own hand-off — delegation off entirely, by a knob that
        // reads like a depth — and anything past the ceiling is a runaway with
        // a number in front of it, since the per-turn fan-out cap applies at
        // every level.
        if let Some(depth) = self.tools.max_delegation_depth
            && !MAX_DELEGATION_DEPTH_BOUNDS.contains(&depth)
        {
            problems.push(format!(
                "`[tools].max_delegation_depth` must be between {} and {} — you wrote `{depth}`. Use `1` to stop desks re-delegating at all.",
                MAX_DELEGATION_DEPTH_BOUNDS.start(),
                MAX_DELEGATION_DEPTH_BOUNDS.end(),
            ));
        }

        if !POLICY_MODES.contains(&self.policy.mode.as_str()) {
            problems.push(one_of("`[policy].mode`", POLICY_MODES, &self.policy.mode));
        }

        if let Some(under) = self.policy.auto_approve_under_usd
            && under < 0.0
        {
            problems.push(format!(
                "`[policy].auto_approve_under_usd` cannot be negative — you wrote `{under}`."
            ));
        }

        for name in self.channels.keys() {
            if !KNOWN_CHANNELS.contains(&name.as_str()) {
                problems.push(format!(
                    "`[channels.{name}]` is not a channel OpenCompany knows — expected one of {}.",
                    join_backticked(KNOWN_CHANNELS)
                ));
            }
        }

        if self.place.discoverable && self.company.handle.is_none() {
            problems.push(
                "`[place].discoverable` is true but `[company].handle` is not set — a public company needs a @handle.".into(),
            );
        }

        for skill in &self.place.skills {
            if parse_usd(&skill.price_usd).is_none() {
                problems.push(format!(
                    "skill `{}` has an invalid `price_usd` `{}` — use a decimal string like \"25.00\".",
                    skill.id, skill.price_usd
                ));
            }
        }

        if let Some(monthly) = self.budget.monthly_usd
            && monthly < 0.0
        {
            problems.push(format!(
                "`[budget].monthly_usd` cannot be negative — you wrote `{monthly}`."
            ));
        }

        // `[plan]` — capability tier gating (issue #108). Only checked when the
        // section is set; an absent `[plan]` leaves gating off and is always ok.
        if self.plan.is_set() {
            if let Some(name) = self.plan.name.as_deref().map(str::trim)
                && !name.is_empty()
                && !PLAN_NAMES.contains(&name)
            {
                problems.push(one_of("`[plan].name`", &PLAN_NAMES, name));
            }
            if !PLAN_PERIODS.contains(&self.plan.period.as_str()) {
                problems.push(one_of("`[plan].period`", &PLAN_PERIODS, &self.plan.period));
            }
            for namespace in self.plan.token_budgets.keys() {
                if !GATEABLE_NAMESPACES.contains(&namespace.as_str()) {
                    problems.push(format!(
                        "`[plan].token_budgets` has an unknown tool namespace `{namespace}` — budget one of {}.",
                        join_backticked(&GATEABLE_NAMESPACES)
                    ));
                }
            }
        }

        for (index, schedule) in self.schedules.iter().enumerate() {
            let fields = schedule.cron.split_whitespace().count();
            if fields != 5 {
                problems.push(format!(
                    "schedule #{} has an invalid `cron` `{}` — a schedule needs 5 fields (minute hour day month weekday).",
                    index + 1,
                    schedule.cron
                ));
            }
        }

        // `[globals].disable`: every entry must name a global that exists. An
        // opt-out that matches nothing is the one failure mode this list must
        // not have — the operator wrote it, believed it, and would still get the
        // global.
        for entry in &self.globals.disable {
            if crate::globals::has(entry) {
                continue;
            }
            match entry.split_once(':') {
                Some((kind, _)) if !crate::globals::DISABLE_KINDS.contains(&kind) => {
                    problems.push(format!(
                        "`[globals].disable` entry `{entry}` has an unknown kind `{kind}` — use one of {}.",
                        join_backticked(crate::globals::DISABLE_KINDS)
                    ));
                }
                Some(_) => problems.push(format!(
                    "`[globals].disable` entry `{entry}` names no global — there is nothing to disable."
                )),
                None => problems.push(format!(
                    "`[globals].disable` entry `{entry}` is missing its kind — write `<kind>:<id>`, e.g. `agent:{entry}`."
                )),
            }
        }

        problems
    }

    /// Validates `[users]`: the sign-in mode, and the bootstrap list that mode
    /// actually reads.
    ///
    /// Split out because the interesting failures are not malformed values but
    /// **silently unread ones**. Each mode reads exactly one bootstrap list —
    /// `admins` in `email`, `wallets` in `wallet`, neither in `none` — so a list
    /// filled in under the wrong mode is not a harmless leftover: it is an
    /// operator who believes they have granted someone access and has not, and
    /// the symptom is an eligible-looking address that can never sign in.
    /// Validates the `[[harness]]` block and every agent's binding to it.
    ///
    /// A section on the wrong kind is an **error, not an ignored key**, for the
    /// same reason a bundle carrying both roster forms is: a silently discarded
    /// declaration stays invisible until the thing it configured misbehaves, and
    /// "my model setting does nothing" is a very expensive way to learn that
    /// `[harness.inference]` needs `kind = "built_in"`.
    fn validate_harnesses(&self) -> Vec<String> {
        let mut problems = Vec::new();

        // An absent block is the implicit built_in harness, which is always
        // valid — and is what every shipped company has. Nothing to check
        // beyond a binding to something that cannot resolve: a coding CLI this
        // build drives locally needs no declaration (see
        // `Harness::implicit_local`), so only a *different* name is a problem.
        if self.harnesses.is_empty() {
            if let Some(agent) = self.agents.iter().find(|a| {
                a.harness
                    .as_deref()
                    .map(str::trim)
                    .is_some_and(|named| !Harness::is_implicit_local_id(named))
            }) {
                let named = agent.harness.as_deref().unwrap_or_default();
                problems.push(format!(
                    "agent `{}` names harness `{named}`, but the manifest declares no `[[harness]]`. \
                     Declare it, or drop the `harness` field to use the built-in default.",
                    agent.id
                ));
            }
            return problems;
        }

        let mut seen: BTreeSet<&str> = BTreeSet::new();
        for harness in &self.harnesses {
            let id = harness.id.trim();
            if id.is_empty() {
                problems.push("`[[harness]]` entries must each set a non-empty `id`.".into());
            } else if !is_snake_case(id) {
                problems.push(format!(
                    "`[[harness]].id` `{id}` is invalid — use snake_case, the same shape as an agent id."
                ));
            } else if !seen.insert(id) {
                problems.push(format!(
                    "`[[harness]].id` `{id}` is declared more than once — harness ids must be unique."
                ));
            }

            if !HARNESS_KINDS.contains(&harness.kind.as_str()) {
                problems.push(one_of(
                    &format!("`[[harness]]` `{id}`'s `kind`"),
                    HARNESS_KINDS,
                    &harness.kind,
                ));
                // The per-kind checks below all read `kind`; with an unknown one
                // they would report confusing follow-on problems.
                continue;
            }

            match harness.kind.as_str() {
                "built_in" => {
                    if harness.acp.is_some() {
                        problems.push(format!(
                            "`[[harness]]` `{id}` is `kind = \"built_in\"` but declares `[harness.acp]`. \
                             An embedded harness has no ACP transport — set `kind = \"acp\"` or drop the section."
                        ));
                    }
                }
                "acp" => {
                    if harness.inference.is_some() {
                        problems.push(format!(
                            "`[[harness]]` `{id}` is `kind = \"acp\"` but declares `[harness.inference]`. \
                             An ACP agent runs on its own credential — drop the section, or use `kind = \"built_in\"`."
                        ));
                    }
                    problems.extend(self.validate_acp_harness(id, harness));
                }
                _ => unreachable!("kind was checked against HARNESS_KINDS above"),
            }
        }

        let defaults = self.harnesses.iter().filter(|h| h.default).count();
        if defaults == 0 {
            problems.push(format!(
                "no `[[harness]]` sets `default = true` — exactly one must, so an agent naming no \
                 harness has somewhere to run. Candidates: {}.",
                join_backticked(
                    &self
                        .harnesses
                        .iter()
                        .map(|h| h.id.as_str())
                        .collect::<Vec<_>>()
                )
            ));
        } else if defaults > 1 {
            problems.push(format!(
                "{defaults} `[[harness]]` entries set `default = true` — exactly one must: {}.",
                join_backticked(
                    &self
                        .harnesses
                        .iter()
                        .filter(|h| h.default)
                        .map(|h| h.id.as_str())
                        .collect::<Vec<_>>()
                )
            ));
        }

        for agent in &self.agents {
            let Some(named) = agent.harness.as_deref().map(str::trim) else {
                continue;
            };
            // A coding CLI this build drives locally needs no declaration —
            // whether it is installed is a fact about the machine, not the
            // blueprint (see `Harness::implicit_local`).
            if !seen.contains(named) && !Harness::is_implicit_local_id(named) {
                problems.push(format!(
                    "agent `{}` names harness `{named}`, which no `[[harness]]` declares. Declared: {}.",
                    agent.id,
                    join_backticked(&seen.iter().copied().collect::<Vec<_>>())
                ));
            }
        }

        // Issue #1245's per-agent follow-up: `agent.model` only means anything
        // on an `acp` harness, exactly like `[harness.acp].model` above — see
        // `validate_acp_harness`'s own doctrine on why silently accepting it
        // elsewhere is worse than refusing it. Skipped when the agent names an
        // unknown harness: the loop above already reports that, and piling a
        // second, confusing complaint about its model on top would not help.
        for agent in &self.agents {
            let Some(model) = agent.model.as_deref() else {
                continue;
            };
            if model.trim().is_empty() {
                problems.push(format!(
                    "agent `{}`'s `model` is set but empty. Drop the key to use the harness's \
                     own default, rather than naming an empty one.",
                    agent.id
                ));
                continue;
            }
            match self.harness_for(&agent.id) {
                Some(harness) if harness.kind == "acp" => {
                    if harness.acp.as_ref().map(|a| a.transport.as_str()) == Some("runner") {
                        problems.push(format!(
                            "agent `{}` names a `model` but its harness `{}` uses \
                             `transport = \"runner\"`. Model overrides aren't supported for a \
                             runner yet — the runner wire protocol doesn't carry them.",
                            agent.id, harness.id
                        ));
                    }
                }
                Some(harness) => {
                    problems.push(format!(
                        "agent `{}` names a `model` but runs on harness `{}` (`kind = \"{}\"`), \
                         which has no ACP transport to forward it to. Bind this agent to an \
                         `acp` harness, or drop `model`.",
                        agent.id, harness.id, harness.kind
                    ));
                }
                None => {}
            }
        }

        problems
    }

    /// The `[harness.acp]` cross-field rules: each transport requires its own
    /// addressing field and forbids the other's, so a manifest cannot claim to
    /// spawn a local agent *and* name a remote runner.
    fn validate_acp_harness(&self, id: &str, harness: &Harness) -> Vec<String> {
        let mut problems = Vec::new();
        let Some(acp) = harness.acp.as_ref() else {
            problems.push(format!(
                "`[[harness]]` `{id}` is `kind = \"acp\"` but declares no `[harness.acp]` — \
                 it needs a `transport`."
            ));
            return problems;
        };

        if !ACP_TRANSPORTS.contains(&acp.transport.as_str()) {
            problems.push(one_of(
                &format!("`[[harness]]` `{id}`'s `[harness.acp].transport`"),
                ACP_TRANSPORTS,
                &acp.transport,
            ));
            return problems;
        }

        match acp.transport.as_str() {
            "local" => {
                match acp.agent.as_deref() {
                    None => problems.push(format!(
                        "`[[harness]]` `{id}` uses `transport = \"local\"` but names no `agent` — \
                         one of {}.",
                        join_backticked(ACP_AGENTS)
                    )),
                    Some(agent) if !ACP_AGENTS.contains(&agent) => problems.push(one_of(
                        &format!("`[[harness]]` `{id}`'s `[harness.acp].agent`"),
                        ACP_AGENTS,
                        agent,
                    )),
                    Some(_) => {}
                }
                if acp.runner.is_some() {
                    problems.push(format!(
                        "`[[harness]]` `{id}` uses `transport = \"local\"` but names a `runner`. \
                         A local agent is spawned on this machine — use `transport = \"runner\"` to reach one elsewhere."
                    ));
                }
            }
            "runner" => {
                if acp
                    .runner
                    .as_deref()
                    .map(str::trim)
                    .unwrap_or_default()
                    .is_empty()
                {
                    problems.push(format!(
                        "`[[harness]]` `{id}` uses `transport = \"runner\"` but names no `runner`."
                    ));
                }
                if acp.agent.is_some() {
                    problems.push(format!(
                        "`[[harness]]` `{id}` uses `transport = \"runner\"` but names an `agent`. \
                         A runner advertises the harnesses it can drive — this host does not choose one for it."
                    ));
                }
                if acp.model.is_some() {
                    problems.push(format!(
                        "`[[harness]]` `{id}` uses `transport = \"runner\"` but names a `model`. \
                         Model overrides aren't supported for a runner yet — the runner wire \
                         protocol doesn't carry them."
                    ));
                }
            }
            _ => unreachable!("transport was checked against ACP_TRANSPORTS above"),
        }

        if acp.model.as_deref().is_some_and(|m| m.trim().is_empty()) {
            problems.push(format!(
                "`[[harness]]` `{id}`'s `[harness.acp].model` is set but empty. Drop the key \
                 to use the agent's own default, rather than naming an empty one."
            ));
        }

        problems
    }

    fn validate_users(&self) -> Vec<String> {
        let mut problems = Vec::new();
        let mode = self.users.mode.as_str();
        if !AUTH_MODES.contains(&mode) {
            problems.push(one_of("`[users].mode`", AUTH_MODES, mode));
            // Every check below is mode-dependent, and reporting them against a
            // mode that does not exist would be noise on top of the real error.
            return problems;
        }

        // Wallet addresses are checked with the same decoder the login route
        // uses, so an address this accepts is one a signature can be verified
        // against.
        for address in &self.users.wallets {
            if let Err(err) = decode_wallet_address(address) {
                problems.push(format!("`[users].wallets` has an invalid entry: {err}"));
            }
        }

        // An admin entry is bootstrapped by comparing its normalized form
        // against the identity a login route resolves — the same normalization
        // `LoginIdentity::parse` has to disambiguate from the `wallet:` and
        // `local:` schemes sharing this column. An entry that does not survive
        // normalization as a real mailbox (missing `@`) is not merely useless,
        // it can normalize to `local:owner` — `normalize_email` only lowercases
        // and trims — and a bootstrapped user stored under that exact key would
        // misparse as the `none`-mode local owner identity rather than the
        // email admin it was meant to be. Caught here so it never reaches a
        // running company.
        for admin in &self.users.admins {
            if !crate::ports::users::is_usable_admin_email(admin) {
                problems.push(format!(
                    "`[users].admins` has an invalid entry: `{admin}` does not look like an \
                     email address"
                ));
            }
        }

        match mode {
            "email" if !self.users.wallets.is_empty() => problems.push(
                "`[users].wallets` is only read when `[users].mode` is `wallet`, so these addresses grant nothing. Set the mode, or list the people in `admins` instead."
                    .into(),
            ),
            "wallet" if !self.users.admins.is_empty() => problems.push(
                "`[users].admins` is only read when `[users].mode` is `email`, so these addresses grant nothing. Set the mode, or list the wallets in `wallets` instead."
                    .into(),
            ),
            "none" if !self.users.admins.is_empty() || !self.users.wallets.is_empty() => problems
                .push(
                    "`[users].mode` is `none`, which has no sign-in and no way to add a second person, so `admins`/`wallets` grant nothing. Remove them, or choose `email` or `wallet`."
                        .into(),
                ),
            _ => {}
        }

        problems
    }

    /// Renders a human-readable summary of the effective configuration, used by
    /// `opencompany check` and the example boot banner.
    pub fn effective_summary(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(out, "Company:  {}", self.company.name);
        if let Some(output) = &self.company.output {
            let _ = writeln!(out, "Output:   {output}");
        }
        if let Some(role) = &self.company.human_role {
            let _ = writeln!(out, "You own:  {role}");
        }
        let _ = writeln!(out, "Brain:    {}", self.brain.mode);
        // Always the effective set, so a company with no `[[harness]]` block
        // prints the implicit harness it actually runs on rather than nothing.
        let default_harness = self.default_harness_id();
        let harnesses = self
            .effective_harnesses()
            .iter()
            .map(|h| {
                let marker = if h.id == default_harness { "*" } else { "" };
                format!("{}{marker} ({})", h.id, h.kind)
            })
            .collect::<Vec<_>>()
            .join(", ");
        let _ = writeln!(out, "Harness:  {harnesses}");
        let _ = writeln!(out, "Policy:   {}", self.policy.mode);
        let _ = writeln!(out, "Tools:    {}", self.tools.provider);
        if let Some(monthly) = self.budget.monthly_usd {
            let _ = writeln!(out, "Budget:   ${monthly:.2}/month");
        }
        let _ = writeln!(
            out,
            "Discover: {}",
            if self.place.discoverable {
                "public"
            } else {
                "private"
            }
        );

        let _ = writeln!(out, "\nRoster ({}):", self.agents.len());
        for agent in &self.agents {
            let tier = agent.tier.as_deref().unwrap_or("—");
            let _ = writeln!(out, "  • {:<20} {}  [tier: {}]", agent.id, agent.role, tier);
        }

        if !self.group_chats.is_empty() {
            let _ = writeln!(out, "\nGroup chats ({}):", self.group_chats.len());
            for chat in &self.group_chats {
                let _ = writeln!(out, "  • {:<20} {}", chat.id, chat.name);
            }
        }
        if !self.connections.is_empty() {
            let names: Vec<&str> = self
                .connections
                .iter()
                .map(|c| c.provider.as_str())
                .collect();
            let _ = writeln!(out, "\nConnections: {}", names.join(", "));
        }
        if !self.workflows.enabled.is_empty() {
            let _ = writeln!(out, "\nWorkflows: {}", self.workflows.enabled.join(", "));
        }
        if !self.channels.is_empty() {
            let names: Vec<&str> = self.channels.keys().map(String::as_str).collect();
            let _ = writeln!(out, "\nChannels: {}", names.join(", "));
        }
        if !self.schedules.is_empty() {
            let _ = writeln!(out, "\nSchedules ({}):", self.schedules.len());
            for schedule in &self.schedules {
                let _ = writeln!(out, "  • {}  →  {}", schedule.cron, schedule.prompt);
            }
        }

        out
    }
}

/// True when `id` is non-empty, starts with a lowercase letter, and contains
/// only lowercase letters, digits, and underscores.
pub(crate) fn is_snake_case(id: &str) -> bool {
    let mut chars = id.chars();
    match chars.next() {
        Some(first) if first.is_ascii_lowercase() => {}
        _ => return false,
    }
    id.chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// Problems from every agent's `[[agent]].ledgers` grants, checked against
/// `builtin_ledgers`.
///
/// An `access = "record"` grant that a built-in ledger's `writers` excludes is
/// a manifest error rather than a silent tool refusal at call time — the two
/// sources of truth (the agent's grant, the ledger's `writers`) must not
/// disagree for a slug the manifest can actually see. A company-declared
/// ledger is not checked here: it may not exist yet when the manifest is
/// validated (the same reasoning as `context`'s missing-document rule), so any
/// disagreement there surfaces as an ordinary tool refusal at call time
/// instead. A free function, not a `CompanyManifest` method, so it can be
/// pointed at a synthetic ledger list in a test without a real registry.
fn ledger_grant_problems(
    agents: &[crate::company::Agent],
    builtin_ledgers: &[crate::ledger::LedgerSpec],
) -> Vec<String> {
    let mut problems = Vec::new();
    for agent in agents {
        let label = if agent.id.is_empty() {
            "an agent".to_string()
        } else {
            format!("agent `{}`", agent.id)
        };
        let Some(grants) = &agent.ledgers else {
            continue;
        };
        for grant in grants {
            if grant.access != crate::company::LedgerAccess::Record {
                continue;
            }
            let Some(spec) = builtin_ledgers
                .iter()
                .find(|spec| spec.slug.eq_ignore_ascii_case(grant.name.trim()))
            else {
                continue;
            };
            if !spec.writable_by(&agent.id) {
                problems.push(format!(
                    "{label} declares `ledgers` access `record` to `{}`, but that ledger's \
                     `writers` does not name this agent — the two must agree. Either add `{}` to \
                     `{}`'s `writers`, or change this grant to `read`.",
                    spec.slug, agent.id, spec.slug
                ));
            }
        }
    }
    problems
}

/// Parses a decimal USD string, rejecting anything non-numeric or negative.
fn parse_usd(value: &str) -> Option<f64> {
    match value.trim().parse::<f64>() {
        Ok(amount) if amount >= 0.0 && amount.is_finite() => Some(amount),
        _ => None,
    }
}

/// Builds a "must be one of … — you wrote `x`" message.
fn one_of(field: &str, allowed: &[&str], actual: &str) -> String {
    format!(
        "{field} must be one of {} — you wrote `{actual}`.",
        allowed.join(", ")
    )
}

fn join_backticked(values: &[&str]) -> String {
    values
        .iter()
        .map(|v| format!("`{v}`"))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> CompanyManifest {
        toml::from_str(text).expect("valid toml")
    }

    /// A valid 32-byte base58 address, built rather than pasted so the test
    /// cannot drift from what the decoder accepts.
    fn wallet_address() -> String {
        bs58::encode([9u8; 32]).into_string()
    }

    /// Writes a company bundle: `company.toml` plus optional `agents/` files.
    fn write_bundle(company_toml: &str, agent_files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join(MANIFEST_FILE), company_toml).expect("write manifest");
        if !agent_files.is_empty() {
            let agents = dir.path().join(super::super::agent_file::AGENTS_DIR);
            std::fs::create_dir_all(&agents).expect("agents dir");
            for (name, body) in agent_files {
                std::fs::write(agents.join(name), body).expect("write agent");
            }
        }
        dir
    }

    /// The compatibility rule: a bare `company.toml` with `[[agent]]` entries
    /// and no `agents/` directory keeps working exactly as it always has.
    #[test]
    fn an_inline_roster_still_parses_when_there_is_no_agents_directory() {
        let dir = write_bundle(
            "[company]\nname = \"X\"\n\n[[agent]]\nid = \"ceo\"\nrole = \"CEO\"\n",
            &[],
        );
        let manifest = CompanyManifest::from_path(dir.path()).expect("parses");
        // The global baseline is appended to every roster, so this asserts the
        // company's own teammates — the thing this test is about.
        let own: Vec<&str> = manifest
            .agents
            .iter()
            .filter(|agent| !agent.global)
            .map(|agent| agent.id.as_str())
            .collect();
        assert_eq!(own, ["ceo"]);
    }

    /// The bundle roster replaces the inline one — so a company that has moved
    /// to `agents/*.toml` gets exactly those teammates.
    #[test]
    fn a_bundle_roster_supplies_the_agents() {
        let dir = write_bundle(
            "[company]\nname = \"X\"\n",
            &[
                ("ceo.toml", "role = \"CEO\"\ntier = \"orchestrator\"\n"),
                ("writer.toml", "role = \"Writer\"\n"),
            ],
        );
        let manifest = CompanyManifest::from_path(dir.path()).expect("parses");
        let ids: Vec<&str> = manifest
            .agents
            .iter()
            .filter(|a| !a.global)
            .map(|a| a.id.as_str())
            .collect();
        assert_eq!(ids, ["ceo", "writer"]);
        // The globals are appended after the company's own roster and none is
        // tagged `orchestrator`, so who orchestrates is unchanged by them.
        assert_eq!(super::super::orchestrator_id(&manifest.agents), Some("ceo"));
    }

    /// Declaring both forms is refused rather than resolved by precedence:
    /// either precedence rule silently discards teammates somebody wrote down.
    #[test]
    fn declaring_both_roster_forms_is_refused_in_prosumer_language() {
        let dir = write_bundle(
            "[company]\nname = \"X\"\n\n[[agent]]\nid = \"ceo\"\nrole = \"CEO\"\n",
            &[("writer.toml", "role = \"Writer\"\n")],
        );
        let err = CompanyManifest::from_path(dir.path()).expect_err("refused");
        let problems = match err {
            OpenCompanyError::ManifestInvalid { problems, .. } => problems,
            other => panic!("expected ManifestInvalid, got {other}"),
        };
        assert_eq!(problems.len(), 1);
        // It must name both places and say what to do, not merely that something
        // is wrong: the operator has to know which half to delete.
        assert!(problems[0].contains("agents/*.toml"), "{problems:?}");
        assert!(problems[0].contains("[[agent]]"), "{problems:?}");
        assert!(problems[0].contains("company.toml"), "{problems:?}");
    }

    /// `opencompany check` must load the bundle roster too. It calls
    /// [`discover`] itself (for the legacy-filename note) and so takes its own
    /// route into loading — which is exactly how it came to validate a manifest
    /// whose roster it had never read, reporting every desk member as missing
    /// from the roster.
    #[test]
    fn run_check_accepts_a_bundle_roster() {
        let dir = write_bundle(
            "[company]\nname = \"X\"\n\n[[group_chat]]\nid = \"d\"\nname = \"D\"\nmembers = [\"ceo\"]\n",
            &[("ceo.toml", "role = \"CEO\"\n")],
        );
        assert!(
            super::super::run_check(dir.path()),
            "a bundle-roster company must validate through the check command"
        );
    }

    /// Cross-cutting validation still applies to a bundle roster: a
    /// `delegates_to` target is checked against the desks in `company.toml`,
    /// which the per-file loader cannot see on its own.
    #[test]
    fn a_bundle_roster_is_still_validated_against_the_rest_of_the_manifest() {
        let dir = write_bundle(
            "[company]\nname = \"X\"\n\n[[group_chat]]\nid = \"research\"\nname = \"Research\"\n",
            &[(
                "ceo.toml",
                "role = \"CEO\"\ndelegates_to = [\"marketing\"]\n",
            )],
        );
        let err = CompanyManifest::from_path(dir.path()).expect_err("refused");
        let problems = match err {
            OpenCompanyError::ManifestInvalid { problems, .. } => problems,
            other => panic!("expected ManifestInvalid, got {other}"),
        };
        assert!(
            problems.iter().any(|p| p.contains("marketing")),
            "{problems:?}"
        );
    }

    #[test]
    fn an_unknown_agent_class_is_refused() {
        let manifest = parse(
            "[company]\nname = \"X\"\n\n[[agent]]\nid = \"critic\"\nrole = \"Critic\"\nclasses = [\"judgey\"]\n",
        );
        let problems = manifest.validate();
        assert!(
            problems
                .iter()
                .any(|p| p.contains("classes") && p.contains("judgey")),
            "{problems:?}"
        );
    }

    #[test]
    fn the_known_agent_classes_are_accepted() {
        let manifest = parse(
            "[company]\nname = \"X\"\n\n[[agent]]\nid = \"critic\"\nrole = \"Critic\"\nclasses = [\"judge\", \"evidence\", \"directive\"]\n",
        );
        assert!(manifest.validate().is_empty(), "{:?}", manifest.validate());
    }

    /// A desk `tools` ceiling is optional and absent by default, so every
    /// manifest written before desks could scope tools keeps its meaning.
    #[test]
    fn a_desk_tool_ceiling_defaults_to_empty() {
        let manifest = parse(
            "[company]\nname = \"X\"\n\n[[agent]]\nid = \"ceo\"\nrole = \"CEO\"\n\n[[group_chat]]\nid = \"d\"\nname = \"D\"\nmembers = [\"ceo\"]\n",
        );
        assert!(manifest.group_chats[0].tools.is_empty());
        assert!(manifest.validate().is_empty());
    }

    /// A manifest naming no `[users].mode` signs people in by email, exactly as
    /// every manifest did before the key existed.
    #[test]
    fn users_mode_defaults_to_email() {
        let manifest = parse("[company]\nname = \"X\"\n");
        assert_eq!(manifest.users.mode, "email");
        assert!(manifest.validate().is_empty());
    }

    #[test]
    fn an_unknown_users_mode_is_named_in_prosumer_language() {
        let manifest = parse("[company]\nname = \"X\"\n[users]\nmode = \"walet\"\n");
        let problems = manifest.validate();
        assert!(
            problems
                .iter()
                .any(|p| p.contains("`[users].mode`") && p.contains("walet")),
            "{problems:?}"
        );
    }

    /// The interesting failure is not a malformed value but a **silently
    /// unread** one: each mode reads exactly one bootstrap list, and filling in
    /// the other is an operator who believes they granted access and has not.
    #[test]
    fn a_bootstrap_list_the_mode_never_reads_is_a_problem() {
        let manifest = parse(&format!(
            "[company]\nname = \"X\"\n[users]\nmode = \"email\"\nwallets = [\"{}\"]\n",
            wallet_address()
        ));
        let problems = manifest.validate();
        assert!(
            problems.iter().any(|p| p.contains("`[users].wallets`")),
            "{problems:?}"
        );

        let manifest =
            parse("[company]\nname = \"X\"\n[users]\nmode = \"wallet\"\nadmins = [\"a@b.com\"]\n");
        assert!(
            manifest
                .validate()
                .iter()
                .any(|p| p.contains("`[users].admins`")),
            "{:?}",
            manifest.validate()
        );
    }

    /// `none` reads neither list, because it admits nobody but the person at the
    /// machine and has no way to add a second.
    #[test]
    fn none_mode_reads_no_bootstrap_list_at_all() {
        let manifest =
            parse("[company]\nname = \"X\"\n[users]\nmode = \"none\"\nadmins = [\"a@b.com\"]\n");
        let problems = manifest.validate();
        assert!(
            problems.iter().any(|p| p.contains("no sign-in")),
            "{problems:?}"
        );

        // Naming no list is the correct `none` manifest, and validates clean.
        let manifest = parse("[company]\nname = \"X\"\n[users]\nmode = \"none\"\n");
        assert!(manifest.validate().is_empty(), "{:?}", manifest.validate());
    }

    /// A wallet that cannot be decoded can never verify a signature, so it is
    /// caught by `opencompany check` rather than by a person who cannot sign in.
    #[test]
    fn a_malformed_bootstrap_wallet_is_rejected() {
        let manifest =
            parse("[company]\nname = \"X\"\n[users]\nmode = \"wallet\"\nwallets = [\"0OIl\"]\n");
        let problems = manifest.validate();
        assert!(
            problems.iter().any(|p| p.contains("`[users].wallets`")),
            "{problems:?}"
        );

        let manifest = parse(&format!(
            "[company]\nname = \"X\"\n[users]\nmode = \"wallet\"\nwallets = [\"{}\"]\n",
            wallet_address()
        ));
        assert!(manifest.validate().is_empty(), "{:?}", manifest.validate());
    }

    /// `normalize_email` only lowercases and trims, so an `[users].admins`
    /// entry with no `@` can still be a normalized key — including one that
    /// collides with the `local:owner` scheme `LoginIdentity::parse` reserves
    /// for the `none`-mode owner. Caught here, before a bootstrapped user is
    /// ever stored under that exact key.
    #[test]
    fn a_bootstrap_admin_that_is_not_an_email_address_is_rejected() {
        let manifest = parse(
            "[company]\nname = \"X\"\n[users]\nmode = \"email\"\nadmins = [\"Local:Owner\"]\n",
        );
        let problems = manifest.validate();
        assert!(
            problems.iter().any(|p| p.contains("`[users].admins`")),
            "{problems:?}"
        );

        let manifest = parse(
            "[company]\nname = \"X\"\n[users]\nmode = \"email\"\nadmins = [\"ada@example.com\"]\n",
        );
        assert!(manifest.validate().is_empty(), "{:?}", manifest.validate());
    }

    #[test]
    fn bare_agents_toml_is_valid() {
        let manifest = parse(
            r#"
            [company]
            name = "Agentic Marketing Agency"
            output = "Campaigns across every channel"
            human_role = "Campaign review and sign-off"

            [[agent]]
            id = "copywriter"
            role = "Copywriter"
            description = "Write ads."
            "#,
        );
        assert!(manifest.validate().is_empty(), "{:?}", manifest.validate());
    }

    #[test]
    fn defaults_are_prosumer_safe() {
        let manifest = parse("[company]\nname = \"Solo\"\n");
        assert_eq!(manifest.brain.mode, "hosted");
        assert_eq!(manifest.tools.provider, "openhuman");
        assert_eq!(manifest.policy.mode, "supervised");
        assert!(!manifest.place.discoverable);
        // Issue #684: this asserted the three-string default verbatim, which is
        // how the defect survived — the list's *contents* were pinned and its
        // *effect* never was, so a list that matched nothing passed. It is
        // empty now, and what makes the defaults prosumer-safe is the
        // `supervised` mode asserted above: `evaluate_supervised` parks every
        // Spend / Sign / Publish effect on its own.
        assert!(
            manifest.policy.always_approve.is_empty(),
            "the default always-approve list is empty on purpose — see \
             DEFAULT_ALWAYS_APPROVE"
        );
    }

    #[test]
    fn workflows_run_cap_defaults_when_omitted() {
        // Issue #401: an absent `[workflows].max_in_flight_runs` takes the
        // generous default and never trips validation.
        let manifest = parse("[company]\nname = \"X\"\n");
        assert_eq!(
            manifest.workflows.max_in_flight_runs,
            crate::company::types::DEFAULT_MAX_IN_FLIGHT_RUNS
        );
        assert!(manifest.validate().is_empty(), "{:?}", manifest.validate());
    }

    #[test]
    fn workflows_run_cap_parses_explicit_value() {
        let manifest = parse("[company]\nname = \"X\"\n[workflows]\nmax_in_flight_runs = 3\n");
        assert_eq!(manifest.workflows.max_in_flight_runs, 3);
        assert!(manifest.validate().is_empty(), "{:?}", manifest.validate());
    }

    #[test]
    fn workflows_run_cap_of_zero_is_rejected() {
        // Issue #401: `0` would refuse every run, so it is a validation error
        // named in prosumer language rather than a silently wedged company.
        let manifest = parse("[company]\nname = \"X\"\n[workflows]\nmax_in_flight_runs = 0\n");
        let problems = manifest.validate();
        assert!(
            problems
                .iter()
                .any(|p| p.contains("`[workflows].max_in_flight_runs`")
                    && p.contains("at least 1")),
            "{problems:?}"
        );
    }

    #[test]
    fn valid_plan_section_passes() {
        let manifest = parse(
            "[company]\nname = \"X\"\n[plan]\nname = \"starter\"\nperiod = \"monthly\"\n[plan.token_budgets]\nweb = 500000\n",
        );
        assert!(manifest.validate().is_empty(), "{:?}", manifest.validate());
    }

    #[test]
    fn absent_plan_is_valid() {
        // No `[plan]` → gating off; the default section must not trip validation.
        let manifest = parse("[company]\nname = \"X\"\n");
        assert!(manifest.validate().is_empty(), "{:?}", manifest.validate());
    }

    #[test]
    fn rejects_unknown_plan_name_in_prosumer_language() {
        let manifest = parse("[company]\nname = \"X\"\n[plan]\nname = \"enterprise\"\n");
        let problems = manifest.validate();
        assert!(
            problems.iter().any(|p| p.contains("`[plan].name`")
                && p.contains("free, starter, pro, unlimited")
                && p.contains("enterprise")),
            "{problems:?}"
        );
    }

    #[test]
    fn rejects_bad_plan_period() {
        let manifest =
            parse("[company]\nname = \"X\"\n[plan]\nname = \"free\"\nperiod = \"hourly\"\n");
        let problems = manifest.validate();
        assert!(
            problems
                .iter()
                .any(|p| p.contains("`[plan].period`") && p.contains("hourly")),
            "{problems:?}"
        );
    }

    #[test]
    fn rejects_non_gateable_budget_namespace() {
        let manifest = parse(
            "[company]\nname = \"X\"\n[plan]\nname = \"pro\"\n[plan.token_budgets]\ntelepathy = 100\n",
        );
        let problems = manifest.validate();
        assert!(
            problems
                .iter()
                .any(|p| p.contains("telepathy") && p.contains("token_budgets")),
            "{problems:?}"
        );
    }

    /// Each tier is accepted by name from a `company.toml` (issue #560).
    ///
    /// This is the test for the trap that adding `auto` set. The validator keeps
    /// its own list of modes (`POLICY_MODES`) and runs *before*
    /// `PolicyMode::parse` ever sees the string, so a tier added to the enum and
    /// the parser but not to that list is rejected at load with "must be one of
    /// …" — unreachable from the only place anybody sets it, while every test in
    /// `harness::policy` still passes because they all construct a `Policy`
    /// directly and never cross this boundary.
    ///
    /// The mode words are **literals** on purpose. Deriving them from
    /// `POLICY_MODES` — the first version of this test — passes vacuously when a
    /// mode is missing from that list, because the missing case simply stops
    /// being generated. Revert-and-check caught it; the literal cannot be
    /// removed by the edit it is meant to detect.
    ///
    /// `harness::policy` holds the matching direction: that `POLICY_MODES` and
    /// the enum agree, so a tier cannot be added here and nowhere else.
    #[test]
    fn every_tier_is_accepted_by_name_from_a_company_toml() {
        for mode in ["readonly", "supervised", "auto", "full"] {
            let manifest = parse(&format!(
                "[company]\nname = \"X\"\n[policy]\nmode = \"{mode}\"\n"
            ));
            let problems = manifest.validate();
            assert!(
                problems.is_empty(),
                "`[policy].mode = \"{mode}\"` is a tier the runtime knows but the manifest \
                 validator rejects — unreachable from a company.toml: {problems:?}"
            );
        }
    }

    /// An `access = "record"` grant to a built-in ledger whose `writers`
    /// excludes this agent must not silently disagree — it is a manifest
    /// error, not a refusal the agent discovers at call time.
    #[test]
    fn a_record_grant_disagreeing_with_a_builtins_writers_is_rejected() {
        let agents = vec![toml::from_str::<crate::company::Agent>(
            "id = \"intern\"\nrole = \"Intern\"\nledgers = [{ name = \"risks\", access = \"record\" }]\n",
        )
        .unwrap()];
        let risks = crate::ledger::parse(
            &serde_json::json!({
                "slug": "risks",
                "title": "Risks",
                "fields": [
                    { "name": "id", "role": "id" },
                    { "name": "risk", "role": "title" },
                    { "name": "status", "role": "status" }
                ],
                "statuses": [{ "name": "open" }, { "name": "closed", "closed": true }],
                "writers": ["cfo"]
            }),
            true,
        )
        .unwrap();

        let problems = ledger_grant_problems(&agents, &[risks]);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("agent `intern`"), "{}", problems[0]);
        assert!(problems[0].contains("`risks`"), "{}", problems[0]);
        assert!(problems[0].contains("writers"), "{}", problems[0]);
    }

    /// A `read` grant never conflicts with `writers` — only `record` implies
    /// write access, so only `record` is checked.
    #[test]
    fn a_read_grant_never_conflicts_with_writers() {
        let agents = vec![toml::from_str::<crate::company::Agent>(
            "id = \"intern\"\nrole = \"Intern\"\nledgers = [{ name = \"risks\", access = \"read\" }]\n",
        )
        .unwrap()];
        let risks = crate::ledger::parse(
            &serde_json::json!({
                "slug": "risks",
                "title": "Risks",
                "fields": [
                    { "name": "id", "role": "id" },
                    { "name": "risk", "role": "title" },
                    { "name": "status", "role": "status" }
                ],
                "statuses": [{ "name": "open" }, { "name": "closed", "closed": true }],
                "writers": ["cfo"]
            }),
            true,
        )
        .unwrap();

        assert!(ledger_grant_problems(&agents, &[risks]).is_empty());
    }

    /// A `delegates_to` entry must name a real desk (issue #176).
    ///
    /// The failure this catches is silent at runtime rather than loud: a member
    /// whose allowlist resolves to nothing still carries `delegate_to_desk`, and
    /// every call it makes is refused as off-allowlist. The manifest is where
    /// that is visible.
    #[test]
    fn rejects_a_delegates_to_entry_that_is_not_a_desk() {
        let manifest = parse(
            "[company]\nname = \"X\"\n\
             [[agent]]\nid = \"lead\"\nrole = \"Lead\"\ndelegates_to = [\"writer\"]\n\
             [[agent]]\nid = \"writer\"\nrole = \"Writer\"\n\
             [[group_chat]]\nid = \"content\"\nname = \"Content desk\"\nmembers = [\"writer\"]\n",
        );
        let problems = manifest.validate();
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("agent `lead`"), "{}", problems[0]);
        assert!(problems[0].contains("`writer`"), "{}", problems[0]);
        // The most common mistake is naming the teammate instead of the desk,
        // so the message has to say which vocabulary the field takes.
        assert!(problems[0].contains("teammate ids"), "{}", problems[0]);
    }

    /// Desk **ids**, desk **names**, and the `"*"` wildcard all resolve; an
    /// empty entry is called out separately from an unknown one.
    #[test]
    fn accepts_desk_ids_names_and_the_wildcard_in_delegates_to() {
        let ok = parse(
            "[company]\nname = \"X\"\n\
             [[agent]]\nid = \"lead\"\nrole = \"Lead\"\ndelegates_to = [\"content\", \"Legal desk\", \"*\"]\n\
             [[group_chat]]\nid = \"content\"\nname = \"Content desk\"\n\
             [[group_chat]]\nid = \"legal\"\nname = \"Legal desk\"\n",
        );
        assert!(ok.validate().is_empty(), "{:?}", ok.validate());

        let blank = parse(
            "[company]\nname = \"X\"\n\
             [[agent]]\nid = \"lead\"\nrole = \"Lead\"\ndelegates_to = [\"  \"]\n\
             [[group_chat]]\nid = \"content\"\nname = \"Content desk\"\n",
        );
        let problems = blank.validate();
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("empty entry"), "{}", problems[0]);
    }

    /// The depth knob is bounded on both sides (issue #176): `0` would mean
    /// "delegation off" wearing a depth's clothes, and past the ceiling the
    /// per-level fan-out cap compounds into a runaway.
    #[test]
    fn rejects_a_delegation_depth_outside_its_bounds() {
        for depth in ["0", "5"] {
            let manifest = parse(&format!(
                "[company]\nname = \"X\"\n[tools]\nmax_delegation_depth = {depth}\n"
            ));
            let problems = manifest.validate();
            assert_eq!(problems.len(), 1, "depth {depth}: {problems:?}");
            assert!(
                problems[0].contains("`[tools].max_delegation_depth`"),
                "{}",
                problems[0]
            );
            assert!(problems[0].contains("between 1 and 4"), "{}", problems[0]);
        }
        for depth in ["1", "2", "3", "4"] {
            let manifest = parse(&format!(
                "[company]\nname = \"X\"\n[tools]\nmax_delegation_depth = {depth}\n"
            ));
            assert!(
                manifest.validate().is_empty(),
                "depth {depth} must be accepted: {:?}",
                manifest.validate()
            );
        }
        // Absent is always fine and means the default.
        let bare = parse("[company]\nname = \"X\"\n");
        assert_eq!(bare.tools.max_delegation_depth, None);
        assert!(bare.validate().is_empty());
    }

    /// An existing manifest that names no `delegates_to` parses to the empty
    /// allowlist, which is what keeps #176 a no-op for every company that did
    /// not ask for it.
    #[test]
    fn delegates_to_defaults_to_empty() {
        let manifest = parse("[company]\nname = \"X\"\n[[agent]]\nid = \"a\"\nrole = \"A\"\n");
        assert!(manifest.agents[0].delegates_to.is_empty());
    }

    #[test]
    fn rejects_bad_policy_mode_in_prosumer_language() {
        let manifest = parse("[company]\nname = \"X\"\n[policy]\nmode = \"supervized\"\n");
        let problems = manifest.validate();
        assert_eq!(problems.len(), 1);
        assert!(problems[0].contains("`[policy].mode`"));
        assert!(problems[0].contains("readonly, supervised, auto, full"));
        assert!(problems[0].contains("supervized"));
    }

    #[test]
    fn rejects_non_snake_case_and_duplicate_ids() {
        let manifest = parse(
            r#"
            [company]
            name = "X"
            [[agent]]
            id = "BadId"
            role = "A"
            [[agent]]
            id = "dup"
            role = "B"
            [[agent]]
            id = "dup"
            role = "C"
            "#,
        );
        let problems = manifest.validate();
        assert!(problems.iter().any(|p| p.contains("snake_case")));
        assert!(problems.iter().any(|p| p.contains("more than once")));
    }

    /// Issue #1757: `operator` is reserved for the built-in, read-only
    /// Operator system channel. A manifest desk claiming it would be
    /// indistinguishable from the system channel in the desk list, and every
    /// message sent there would be refused by `chat_and_emit`'s read-only
    /// guard (`src/server/operator.rs`), which does not know or care where a
    /// `chat_id == OPERATOR_CHANNEL` came from.
    #[test]
    fn rejects_a_group_chat_claiming_the_reserved_operator_id() {
        let manifest = parse(
            "[company]\nname = \"X\"\n\
             [[agent]]\nid = \"ceo\"\nrole = \"CEO\"\n\
             [[group_chat]]\nid = \"operator\"\nname = \"Operator\"\nmembers = [\"ceo\"]\n",
        );
        let problems = manifest.validate();
        assert!(
            problems
                .iter()
                .any(|p| p.contains("reserved") && p.contains("operator")),
            "{problems:?}"
        );
    }

    /// Issue #1781 review (Codex P2): the id check alone is not enough —
    /// `server::operator::resolve_desk` matches a desk by id *or*
    /// case-insensitive name, so a desk at a harmless id but named "Operator"
    /// shadows the system channel exactly as thoroughly as claiming the
    /// literal id would: `GET {scope}/chat/history?desk=operator` (the
    /// console's pinned read-only row) resolves to this desk instead of the
    /// system feed, and its own writable transcript displays through the
    /// identity the console assumes is read-only.
    #[test]
    fn rejects_a_group_chat_named_operator_even_with_a_harmless_id() {
        let manifest = parse(
            "[company]\nname = \"X\"\n\
             [[agent]]\nid = \"ceo\"\nrole = \"CEO\"\n\
             [[group_chat]]\nid = \"ops\"\nname = \"Operator\"\nmembers = [\"ceo\"]\n",
        );
        let problems = manifest.validate();
        assert!(
            problems
                .iter()
                .any(|p| p.contains("reserved") && p.contains("Operator")),
            "{problems:?}"
        );
    }

    /// Case-insensitive, matching `resolve_desk`'s own fold — "operator" and
    /// "OPERATOR" alias the same collision as "Operator" does.
    #[test]
    fn the_operator_name_reservation_folds_case() {
        let manifest = parse(
            "[company]\nname = \"X\"\n\
             [[agent]]\nid = \"ceo\"\nrole = \"CEO\"\n\
             [[group_chat]]\nid = \"ops\"\nname = \"operator\"\nmembers = [\"ceo\"]\n",
        );
        let problems = manifest.validate();
        assert!(
            problems.iter().any(|p| p.contains("reserved")),
            "{problems:?}"
        );
    }

    /// PR #1781 review follow-up: the id/name reservation above only blocks
    /// the literal `OPERATOR_CHANNEL` name ("Operator"), but a grandfathered
    /// collision diverts the durable feed to
    /// `OPERATOR_CHANNEL_COLLISION_FALLBACK` ("operator-feed") instead —
    /// `server::operator::resolve_desk` resolves a `?desk=` selector against
    /// `chat.name.eq_ignore_ascii_case(desk)` with no distinction between the
    /// two addresses. A desk named `operator-feed` therefore still passes
    /// this validation, survives `from_path_for_reload`, and then shadows
    /// the fallback address exactly as thoroughly as a desk literally named
    /// "Operator" would shadow the primary one: `GET
    /// {scope}/chat/history?desk=operator-feed`, the request the console's
    /// pinned Operator row makes once diverted, resolves to this desk instead
    /// of the collision-fallback feed.
    #[test]
    fn the_operator_feed_fallback_name_is_also_reserved() {
        let manifest = parse(
            "[company]\nname = \"X\"\n\
             [[agent]]\nid = \"ceo\"\nrole = \"CEO\"\n\
             [[group_chat]]\nid = \"ops\"\nname = \"operator-feed\"\nmembers = [\"ceo\"]\n",
        );
        let problems = manifest.validate();
        assert!(
            problems
                .iter()
                .any(|p| p.contains("reserved") && p.contains("operator-feed")),
            "{problems:?}"
        );
    }

    /// Follow-up to the group-chat guard above: `RESERVED_AGENT_IDS` already
    /// stops a console-minted teammate from taking `system`
    /// (`mint_agent_id`), but a manifest agent's id is read straight from the
    /// TOML and this loop never consulted the same list — so a manifest could
    /// still declare `id = "system"` and collide with the runtime's own
    /// author id (`SYSTEM_AUTHOR`, issue #966): `senderOf` reads `agent_id`
    /// by value and would render every subsequent system notice as that
    /// teammate.
    #[test]
    fn rejects_a_manifest_agent_claiming_a_reserved_id() {
        let manifest = parse(
            "[company]\nname = \"X\"\n\
             [[agent]]\nid = \"system\"\nrole = \"whatever\"\n",
        );
        let problems = manifest.validate();
        assert!(
            problems
                .iter()
                .any(|p| p.contains("reserved") && p.contains("system")),
            "{problems:?}"
        );
    }

    /// The same guard covers every entry in `RESERVED_AGENT_IDS`, not just
    /// `system` — `operator`, `agents`, and `desks` are equally live manifest
    /// agent ids until this check runs.
    ///
    /// Lowercased before use: the reserved-id arm compares
    /// `eq_ignore_ascii_case` on purpose (`RESERVED_AGENT_IDS`'s own doc),
    /// because one entry — `DEFAULT_DESK`, `"General"` — is a prosumer display
    /// string, not a slug. Every manifest agent id must already be snake_case
    /// (checked one arm above this one), so submitting `"General"` verbatim
    /// never reaches the reserved-id arm at all — it is rejected first, and
    /// correctly, as an invalid id format. Lowercasing exercises the guard
    /// through the one shape a manifest id can actually take, for every
    /// reserved value including that one.
    #[test]
    fn rejects_every_reserved_id_as_a_manifest_agent_id() {
        for reserved in crate::ports::types::RESERVED_AGENT_IDS {
            let candidate = reserved.to_ascii_lowercase();
            let manifest = parse(&format!(
                "[company]\nname = \"X\"\n[[agent]]\nid = \"{candidate}\"\nrole = \"whatever\"\n"
            ));
            let problems = manifest.validate();
            assert!(
                problems
                    .iter()
                    .any(|p| p.contains("reserved") && p.to_ascii_lowercase().contains(&candidate)),
                "id {candidate:?} (reserved: {reserved:?}) should have been rejected: {problems:?}"
            );
        }
    }

    /// Issue #1781 review (Codex P1): `register_company`'s `serve` boot loop
    /// reloads every company directory's `company.toml` on each restart, so
    /// a company whose roster already grandfathers a teammate at a
    /// [`RESERVED_AGENT_IDS`](crate::ports::types::RESERVED_AGENT_IDS) id —
    /// `operator`, the case the rest of this codebase's grandfather-support
    /// machinery (`channel.rs`, `operator.rs`, `delivery.rs`) exists to run
    /// correctly — must still be able to boot. `from_path`, the strict
    /// authoring-time loader, is proven first to still refuse it (unchanged
    /// behavior, pinning the pre-fix failure this regresses against);
    /// `from_path_for_reload` must accept the identical manifest.
    #[test]
    fn from_path_for_reload_grandfathers_a_manifest_agent_at_a_reserved_id() {
        let dir = write_bundle(
            "[company]\nname = \"Acme\"\n\n[[agent]]\nid = \"operator\"\nrole = \"Chief of Staff\"\n",
            &[],
        );

        let strict = CompanyManifest::from_path(dir.path());
        assert!(
            strict.is_err(),
            "sanity check: the strict authoring loader must still refuse this manifest, \
             or this test is not exercising the rule it claims to"
        );

        let reloaded = CompanyManifest::from_path_for_reload(dir.path())
            .expect("a company that already grandfathers an `operator` teammate must reboot");
        assert!(
            reloaded.agents.iter().any(|a| a.id == "operator"),
            "the grandfathered agent itself must still be loaded, not merely tolerated: {:?}",
            reloaded.agents
        );
    }

    /// The reload loader still enforces every other manifest rule — it
    /// grandfathers exactly the reserved-agent-id collision, not validation
    /// as a whole, so a company directory hand-edited into a genuinely
    /// invalid shape (here, a duplicate agent id) must still refuse to boot.
    #[test]
    fn from_path_for_reload_still_refuses_an_unrelated_validation_problem() {
        let dir = write_bundle(
            "[company]\nname = \"Acme\"\n\n\
             [[agent]]\nid = \"writer\"\nrole = \"Writer\"\n\n\
             [[agent]]\nid = \"writer\"\nrole = \"Also Writer\"\n",
            &[],
        );

        let err = CompanyManifest::from_path_for_reload(dir.path())
            .expect_err("a duplicate agent id must still be refused on reload");
        assert!(
            format!("{err}").contains("more than once"),
            "unexpected error: {err}"
        );
    }

    /// Issue #1781 review (Codex P1): the `operator` group-chat id/name
    /// reservation (`rejects_a_group_chat_claiming_the_reserved_operator_id`
    /// above) is the desk-side twin of the agent-id reservation
    /// `from_path_for_reload_grandfathers_a_manifest_agent_at_a_reserved_id`
    /// covers — both postdate real companies, since `operator` only became a
    /// reserved system channel with issue #1757. The agent-id arm was gated
    /// on `enforce_reserved_agent_ids`; this arm was not, so a company whose
    /// desk list already declared `id = "operator"` before the reservation
    /// shipped could reboot as an agent-only grandfather case but never as a
    /// desk one — `register_company`'s `serve` boot loop would refuse it on
    /// every restart. `from_path` is proven first to still refuse it
    /// (pinning the pre-fix failure this regresses against);
    /// `from_path_for_reload` must accept the identical manifest and keep
    /// the desk itself loaded.
    #[test]
    fn from_path_for_reload_grandfathers_a_group_chat_at_the_reserved_operator_id() {
        let dir = write_bundle(
            "[company]\nname = \"Acme\"\n\n\
             [[agent]]\nid = \"ceo\"\nrole = \"CEO\"\n\n\
             [[group_chat]]\nid = \"operator\"\nname = \"Legacy Ops\"\nmembers = [\"ceo\"]\n",
            &[],
        );

        let strict = CompanyManifest::from_path(dir.path());
        assert!(
            strict.is_err(),
            "sanity check: the strict authoring loader must still refuse this manifest, \
             or this test is not exercising the rule it claims to"
        );

        let reloaded = CompanyManifest::from_path_for_reload(dir.path())
            .expect("a company that already has a desk at the `operator` id must reboot");
        assert!(
            reloaded.group_chats.iter().any(|c| c.id == "operator"),
            "the grandfathered desk itself must still be loaded, not merely tolerated: {:?}",
            reloaded.group_chats
        );
    }

    /// The name-collision twin of the test above: a desk at a harmless id but
    /// named "Operator" shadows the system channel exactly as thoroughly
    /// (`server::operator::resolve_desk` matches by id *or* case-insensitive
    /// name — see `rejects_a_group_chat_named_operator_even_with_a_harmless_id`),
    /// and was equally unconditional before this fix.
    #[test]
    fn from_path_for_reload_grandfathers_a_group_chat_named_operator() {
        let dir = write_bundle(
            "[company]\nname = \"Acme\"\n\n\
             [[agent]]\nid = \"ceo\"\nrole = \"CEO\"\n\n\
             [[group_chat]]\nid = \"legacy_ops\"\nname = \"Operator\"\nmembers = [\"ceo\"]\n",
            &[],
        );

        let strict = CompanyManifest::from_path(dir.path());
        assert!(
            strict.is_err(),
            "sanity check: the strict authoring loader must still refuse this manifest, \
             or this test is not exercising the rule it claims to"
        );

        let reloaded = CompanyManifest::from_path_for_reload(dir.path())
            .expect("a company that already has a desk named \"Operator\" must reboot");
        assert!(
            reloaded.group_chats.iter().any(|c| c.id == "legacy_ops"),
            "the grandfathered desk itself must still be loaded, not merely tolerated: {:?}",
            reloaded.group_chats
        );
    }

    #[test]
    fn rejects_unknown_channel_and_bad_tier() {
        let manifest = parse(
            r#"
            [company]
            name = "X"
            [[agent]]
            id = "a"
            role = "A"
            tier = "genius"
            [channels.telepathy]
            enabled = true
            "#,
        );
        let problems = manifest.validate();
        assert!(problems.iter().any(|p| p.contains("telepathy")));
        assert!(
            problems
                .iter()
                .any(|p| p.contains("`tier`") && p.contains("genius"))
        );
    }

    #[test]
    fn public_company_requires_handle() {
        let manifest = parse("[company]\nname = \"X\"\n[place]\ndiscoverable = true\n");
        let problems = manifest.validate();
        assert!(problems.iter().any(|p| p.contains("@handle")));
    }

    #[test]
    fn rejects_bad_skill_price_and_cron() {
        let manifest = parse(
            r#"
            [company]
            name = "X"
            handle = "x"
            [place]
            discoverable = true
            skills = [{ id = "seo.audit", price_usd = "free" }]
            [[schedule]]
            cron = "every monday"
            prompt = "review"
            "#,
        );
        let problems = manifest.validate();
        assert!(problems.iter().any(|p| p.contains("price_usd")));
        assert!(problems.iter().any(|p| p.contains("5 fields")));
    }

    #[test]
    fn accepts_group_chats_connections_and_workflows() {
        let manifest = parse(
            r#"
            [company]
            name = "Agentic Marketing Agency"

            [[agent]]
            id = "creative_director"
            role = "Creative Director"
            [[agent]]
            id = "copywriter"
            role = "Copywriter"

            [[group_chat]]
            id = "creative"
            name = "Creative studio"
            description = "Copy, design, and campaigns"
            members = ["creative_director", "copywriter"]

            [[connection]]
            provider = "slack"
            priority = "high"
            scopes = ["chat:write"]
            reason = "Post campaign updates"

            [workflows]
            enabled = ["campaign_pipeline"]
            "#,
        );
        assert!(manifest.validate().is_empty(), "{:?}", manifest.validate());
        assert_eq!(manifest.group_chats.len(), 1);
        assert_eq!(manifest.group_chats[0].members.len(), 2);
        assert_eq!(manifest.connections[0].provider, "slack");
        assert_eq!(manifest.workflows.enabled, vec!["campaign_pipeline"]);
    }

    #[test]
    fn rejects_unknown_member_bad_priority_and_workflow_id() {
        let manifest = parse(
            r#"
            [company]
            name = "X"
            [[agent]]
            id = "a"
            role = "A"

            [[group_chat]]
            id = "team"
            name = "Team"
            members = ["ghost"]

            [[connection]]
            provider = "slack"
            priority = "urgent"

            [workflows]
            enabled = ["Bad-Id"]
            "#,
        );
        let problems = manifest.validate();
        assert!(
            problems
                .iter()
                .any(|p| p.contains("ghost") && p.contains("not an agent")),
            "{problems:?}"
        );
        assert!(
            problems
                .iter()
                .any(|p| p.contains("`priority`") && p.contains("urgent")),
            "{problems:?}"
        );
        assert!(
            problems
                .iter()
                .any(|p| p.contains("workflow id") && p.contains("Bad-Id")),
            "{problems:?}"
        );
    }

    /// A bundle laying an `mcp.json` beside its `company.toml` gets those
    /// servers, and they are held to the same validator an inline entry is.
    #[test]
    fn a_bundle_mcp_json_reaches_the_manifest() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join(MANIFEST_FILE), "[company]\nname = \"X\"\n")
            .expect("write manifest");
        std::fs::write(
            dir.path().join("mcp.json"),
            r#"{"mcpServers": {"deepwiki": {"url": "https://mcp.deepwiki.com/mcp"}}}"#,
        )
        .expect("write mcp.json");

        let manifest = CompanyManifest::from_path(dir.path()).expect("loads");
        let names: Vec<&str> = manifest
            .mcp_servers
            .iter()
            .map(|s| s.name.as_str())
            .collect();
        assert_eq!(names, ["deepwiki"]);
    }

    /// A server declared in both forms is refused rather than resolved by
    /// precedence — the roster's rule, for the roster's reason: either
    /// precedence rule silently discards a declaration somebody wrote down.
    #[test]
    fn a_server_declared_in_both_forms_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join(MANIFEST_FILE),
            "[company]\nname = \"X\"\n[[mcp_server]]\nname = \"deepwiki\"\nendpoint = \"https://one.test/mcp\"\n",
        )
        .expect("write manifest");
        std::fs::write(
            dir.path().join("mcp.json"),
            r#"{"mcpServers": {"deepwiki": {"url": "https://two.test/mcp"}}}"#,
        )
        .expect("write mcp.json");

        let err = CompanyManifest::from_path(dir.path()).expect_err("must refuse");
        let text = err.to_string();
        assert!(text.contains("deepwiki"), "{text}");
    }

    /// A bad entry in `mcp.json` is reported against the manifest rather than
    /// swallowed — the file is genuinely read, and its problems genuinely land.
    #[test]
    fn a_bad_bundle_server_is_reported_against_the_manifest() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join(MANIFEST_FILE), "[company]\nname = \"X\"\n")
            .expect("write manifest");
        std::fs::write(
            dir.path().join("mcp.json"),
            r#"{"mcpServers": {"local": {"command": "npx some-mcp"}}}"#,
        )
        .expect("write mcp.json");

        let err = CompanyManifest::from_path(dir.path()).expect_err("must refuse");
        let text = err.to_string();
        assert!(
            text.contains("stdio") && text.contains("mcp.json"),
            "{text}"
        );
    }

    #[test]
    fn accepts_http_mcp_server_and_rejects_stdio() {
        let ok = parse(
            r#"
            [company]
            name = "X"
            [[mcp_server]]
            name = "notion"
            endpoint = "https://notion.example/mcp"
            "#,
        );
        assert!(ok.validate().is_empty(), "{:?}", ok.validate());

        let bad = parse(
            r#"
            [company]
            name = "X"
            [[mcp_server]]
            name = "local"
            command = "npx some-mcp"
            "#,
        );
        let problems = bad.validate();
        assert!(
            problems
                .iter()
                .any(|p| p.contains("stdio") && p.contains("hosted v1")),
            "{problems:?}"
        );
    }

    #[test]
    fn accepts_byok_inference_and_rejects_bad_provider() {
        let ok = parse(
            r#"
            [company]
            name = "X"
            [inference]
            provider = "openrouter"
            [inference.models]
            "chat-v1" = "deepseek/deepseek-chat"
            "#,
        );
        assert!(ok.validate().is_empty(), "{:?}", ok.validate());
        assert_eq!(ok.inference.provider.as_deref(), Some("openrouter"));
        assert_eq!(
            ok.inference.models.get("chat-v1").map(String::as_str),
            Some("deepseek/deepseek-chat")
        );

        let bad = parse(
            r#"
            [company]
            name = "X"
            [inference]
            provider = "ollama"
            "#,
        );
        // Ollama needs a base_url.
        assert!(
            bad.validate()
                .iter()
                .any(|p| p.contains("base_url") && p.contains("required")),
            "{:?}",
            bad.validate()
        );
    }

    #[test]
    fn effective_summary_lists_roster() {
        let manifest = parse(
            r#"
            [company]
            name = "Agentic Marketing Agency"
            [[agent]]
            id = "copywriter"
            role = "Copywriter"
            "#,
        );
        let summary = manifest.effective_summary();
        assert!(summary.contains("Agentic Marketing Agency"));
        assert!(summary.contains("copywriter"));
        assert!(summary.contains("Roster (1)"));
    }

    #[test]
    fn signals_opportunity_studio_template_passes_lint() {
        // The Signals + Opportunity Engine ship as a venture-studio template,
        // not kernel code. This guards that the shipped manifest keeps passing
        // the same lint `opencompany check` runs — unique agent ids, priced +
        // described `[place].skills`, a `[policy]`, and a stated `human_role`.
        // The company *directory*, not its `company.toml`: this template's
        // roster lives in `agents/*.toml`, and loading the file alone would
        // leave `manifest.agents` empty — making every roster assertion below
        // pass by having nothing to check.
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("companies/signals_opportunity_studio");
        let manifest = CompanyManifest::from_path(&path).expect("template manifest is valid");

        assert!(manifest.validate().is_empty(), "{:?}", manifest.validate());
        assert!(
            !manifest.agents.is_empty(),
            "the roster must actually load, or the assertions below check nothing"
        );
        assert!(
            manifest.company.human_role.is_some(),
            "the template must name what the human keeps"
        );
        // Unique agent ids.
        let mut ids: Vec<&str> = manifest.agents.iter().map(|a| a.id.as_str()).collect();
        ids.sort_unstable();
        let unique = ids.len();
        ids.dedup();
        assert_eq!(ids.len(), unique, "agent ids must be unique");
        // Every advertised skill is priced and described.
        assert!(!manifest.place.skills.is_empty());
        for skill in &manifest.place.skills {
            assert!(
                parse_usd(&skill.price_usd).is_some(),
                "skill must be priced"
            );
            assert!(
                skill
                    .description
                    .as_deref()
                    .is_some_and(|d| !d.trim().is_empty()),
                "skill `{}` must be described",
                skill.id
            );
        }
        // A supervised policy with a defined always-approve fence. Asserting
        // only `!is_empty()` is what let the template ship three entries that
        // matched nothing on its harness path (issue #684): a list's length
        // says nothing about whether it fires.
        assert_eq!(manifest.policy.mode, "supervised");
        assert!(!manifest.policy.always_approve.is_empty());
        // What was actually wrong is that none of the old entries named a tool,
        // and the template runs the openhuman harness. A shipped template must
        // demonstrate a gate that works on its own path, not merely a plausible
        // effect-kind string.
        assert!(
            crate::policy::always_approve::matches(
                &manifest.policy.always_approve,
                "publish_artifact"
            ),
            "the template's fence names no declared tool, so nothing in it can \
             park a harness tool call — the shape of issue #684"
        );
        // The weekly opportunity loop is a schedule.
        assert!(!manifest.schedules.is_empty());
    }

    #[test]
    fn discover_prefers_company_toml() {
        let dir = std::env::temp_dir().join(format!("oc-discover-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(LEGACY_MANIFEST_FILE), "[company]\nname=\"L\"\n").unwrap();
        let located = discover(&dir).unwrap();
        assert!(located.legacy);
        std::fs::write(dir.join(MANIFEST_FILE), "[company]\nname=\"C\"\n").unwrap();
        let located = discover(&dir).unwrap();
        assert!(!located.legacy);
        std::fs::remove_dir_all(&dir).ok();
    }
}

#[cfg(test)]
mod harness_tests {
    use super::*;

    fn parse(text: &str) -> CompanyManifest {
        toml::from_str(text).expect("valid toml")
    }

    /// Every problem mentioning `harness`, so a test asserting on this block is
    /// not perturbed by unrelated validation output.
    fn harness_problems(m: &CompanyManifest) -> Vec<String> {
        m.validate()
            .into_iter()
            .filter(|p| p.contains("harness"))
            .collect()
    }

    const BASE: &str = "[company]\nname = \"X\"\n\n[[agent]]\nid = \"ceo\"\nrole = \"CEO\"\n";

    /// The compatibility case, and the one every shipped company under
    /// `companies/` hits: no `[[harness]]` block at all still yields exactly one
    /// harness — `built_in`, default, on the company-level `[inference]`.
    ///
    /// This is the test that makes "named harnesses" a purely additive feature.
    #[test]
    fn a_manifest_with_no_harness_block_gets_one_implicit_built_in_default() {
        let manifest = parse(BASE);

        assert!(manifest.harnesses.is_empty(), "nothing was declared");

        let effective = manifest.effective_harnesses();
        assert_eq!(effective.len(), 1);
        assert_eq!(effective[0].id, IMPLICIT_HARNESS_ID);
        assert_eq!(effective[0].kind, "built_in");
        assert!(effective[0].default);
        assert!(effective[0].inference.is_none(), "inherits `[inference]`");

        assert_eq!(manifest.default_harness_id(), IMPLICIT_HARNESS_ID);
        assert_eq!(
            manifest.harness_for("ceo").map(|h| h.id),
            Some(IMPLICIT_HARNESS_ID.to_string()),
            "an agent naming no harness lands on the implicit one"
        );
        assert!(harness_problems(&manifest).is_empty());
    }

    /// `default_harness` resolves the same entry `default_harness_id` names,
    /// full struct and all — for both the implicit `built_in` case and a
    /// declared `acp` default. Pinned separately from `default_harness_id`
    /// because `lanes::build` (issue #1244) reads `.kind` off this to decide
    /// whether the default lane is even runnable; a lookup that silently
    /// resolved to the wrong harness would reintroduce the bug that fixed.
    #[test]
    fn default_harness_resolves_the_full_declared_entry() {
        let implicit = parse(BASE);
        assert_eq!(implicit.default_harness().id, IMPLICIT_HARNESS_ID);
        assert_eq!(implicit.default_harness().kind, "built_in");

        let acp_default = parse(&format!(
            "{BASE}\n[[harness]]\nid = \"laptop\"\nkind = \"acp\"\ndefault = true\n\n\
             [harness.acp]\ntransport = \"local\"\nagent = \"claude\"\n"
        ));
        assert_eq!(acp_default.default_harness().id, "laptop");
        assert_eq!(acp_default.default_harness().kind, "acp");
    }

    /// Naming a harness when none is declared is an error rather than a silent
    /// fallback to the implicit one: the operator wrote down an intent, and
    /// quietly ignoring it is how "my agent is on the wrong model" happens.
    #[test]
    fn naming_a_harness_with_no_harness_block_is_rejected() {
        let manifest = parse(&format!("{BASE}harness = \"deep\"\n"));
        let problems = harness_problems(&manifest);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("ceo") && problems[0].contains("deep"));
    }

    #[test]
    fn agents_route_to_their_named_harness_and_others_to_the_default() {
        let manifest = parse(
            r#"
[company]
name = "X"

[[agent]]
id = "ceo"
role = "CEO"

[[agent]]
id = "researcher"
role = "Researcher"
harness = "deep"

[[harness]]
id = "embedded"
kind = "built_in"
default = true

[[harness]]
id = "deep"
kind = "built_in"

[harness.inference]
provider = "openrouter"
"#,
        );
        assert!(harness_problems(&manifest).is_empty());
        assert_eq!(manifest.default_harness_id(), "embedded");
        assert_eq!(
            manifest.harness_for("ceo").map(|h| h.id),
            Some("embedded".to_string())
        );
        assert_eq!(
            manifest.harness_for("researcher").map(|h| h.id),
            Some("deep".to_string())
        );
        // The sub-table attached to the *second* entry, not the first — the
        // array-of-tables shape that is easy to misread.
        let deep = manifest.harness_for("researcher").expect("declared");
        assert_eq!(
            deep.inference.as_ref().and_then(|i| i.provider.clone()),
            Some("openrouter".to_string())
        );
        assert!(
            manifest
                .effective_harnesses()
                .iter()
                .find(|h| h.id == "embedded")
                .expect("declared")
                .inference
                .is_none()
        );
    }

    #[test]
    fn an_agent_naming_an_undeclared_harness_is_rejected() {
        let manifest = parse(&format!(
            "{BASE}harness = \"ghost\"\n\n[[harness]]\nid = \"embedded\"\nkind = \"built_in\"\ndefault = true\n"
        ));
        let problems = harness_problems(&manifest);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("ghost") && problems[0].contains("ceo"));
        assert!(
            problems[0].contains("embedded"),
            "names what IS declared: {}",
            problems[0]
        );
    }

    /// Issue #1245's detected-harness follow-up: whether `claude-agent-acp` is
    /// installed is a fact about the **machine**, so binding to it must not
    /// require a `company.toml` edit — the same manifest is opened from
    /// machines where the answer differs. Accepted with a harness block
    /// declared and without one, and it resolves to a `local` acp harness.
    #[test]
    fn a_coding_cli_is_bindable_without_being_declared() {
        for tail in [
            "",
            "\n[[harness]]\nid = \"embedded\"\nkind = \"built_in\"\ndefault = true\n",
        ] {
            let manifest = parse(&format!("{BASE}harness = \"claude\"\n{tail}"));
            assert!(
                harness_problems(&manifest).is_empty(),
                "`claude` needs no declaration ({tail:?}): {:?}",
                harness_problems(&manifest)
            );

            let resolved = manifest.harness_for("ceo").expect("resolves");
            assert_eq!(resolved.id, "claude");
            assert_eq!(resolved.kind, "acp");
            let acp = resolved.acp.expect("acp section");
            assert_eq!(acp.transport, "local");
            assert_eq!(acp.agent.as_deref(), Some("claude"));
        }
    }

    /// The synthesized harness must never be the default: which harness an
    /// *unbound* teammate runs on stays a blueprint decision, or something a
    /// machine happens to have installed could silently redirect the roster.
    #[test]
    fn an_implicit_local_harness_is_never_the_default() {
        let manifest = parse(&format!("{BASE}harness = \"claude\"\n"));
        assert_ne!(manifest.default_harness_id(), "claude");
        assert!(manifest.default_harness().is_built_in());
        assert!(!Harness::implicit_local("claude").default);
    }

    /// A declared `[[harness]]` of the same id wins — otherwise a company that
    /// deliberately pinned a model on its `claude` harness would silently get
    /// the bare synthesized one instead.
    #[test]
    fn a_declared_harness_wins_over_the_synthesized_one() {
        let manifest = parse(&format!(
            "{BASE}harness = \"claude\"\n\n[[harness]]\nid = \"embedded\"\nkind = \"built_in\"\ndefault = true\n\n\
             [[harness]]\nid = \"claude\"\nkind = \"acp\"\n\n[harness.acp]\ntransport = \"local\"\nagent = \"claude\"\nmodel = \"opus-4-5\"\n"
        ));
        assert!(harness_problems(&manifest).is_empty());
        let resolved = manifest.harness_for("ceo").expect("resolves");
        assert_eq!(
            resolved.acp.expect("acp").model.as_deref(),
            Some("opus-4-5"),
            "the declared harness, not the synthesized one"
        );
    }

    #[test]
    fn duplicate_harness_ids_are_rejected() {
        let manifest = parse(&format!(
            "{BASE}\n[[harness]]\nid = \"a\"\nkind = \"built_in\"\ndefault = true\n\n[[harness]]\nid = \"a\"\nkind = \"built_in\"\n"
        ));
        let problems = harness_problems(&manifest);
        assert!(
            problems.iter().any(|p| p.contains("more than once")),
            "{problems:?}"
        );
    }

    /// Zero and two defaults are both errors. Zero would leave an agent naming
    /// no harness with nowhere to run; two makes the answer depend on list
    /// order, which is exactly what marking a default exists to avoid.
    #[test]
    fn there_must_be_exactly_one_default_harness() {
        let none = parse(&format!(
            "{BASE}\n[[harness]]\nid = \"a\"\nkind = \"built_in\"\n\n[[harness]]\nid = \"b\"\nkind = \"built_in\"\n"
        ));
        let problems = harness_problems(&none);
        assert!(
            problems.iter().any(|p| p.contains("no `[[harness]]` sets")),
            "{problems:?}"
        );
        assert!(
            problems
                .iter()
                .any(|p| p.contains("`a`") && p.contains("`b`")),
            "names the candidates: {problems:?}"
        );

        let two = parse(&format!(
            "{BASE}\n[[harness]]\nid = \"a\"\nkind = \"built_in\"\ndefault = true\n\n[[harness]]\nid = \"b\"\nkind = \"built_in\"\ndefault = true\n"
        ));
        let problems = harness_problems(&two);
        assert!(
            problems.iter().any(|p| p.contains("2 `[[harness]]`")),
            "{problems:?}"
        );
    }

    /// A section on the wrong kind is an error, not an ignored key — both
    /// directions.
    #[test]
    fn a_section_on_the_wrong_kind_is_an_error_not_an_ignored_key() {
        let inference_on_acp = parse(&format!(
            "{BASE}\n[[harness]]\nid = \"a\"\nkind = \"acp\"\ndefault = true\n\n[harness.acp]\ntransport = \"local\"\nagent = \"claude\"\n\n[harness.inference]\nprovider = \"openrouter\"\n"
        ));
        let problems = harness_problems(&inference_on_acp);
        assert!(
            problems
                .iter()
                .any(|p| p.contains("[harness.inference]") && p.contains("own credential")),
            "{problems:?}"
        );

        let acp_on_built_in = parse(&format!(
            "{BASE}\n[[harness]]\nid = \"a\"\nkind = \"built_in\"\ndefault = true\n\n[harness.acp]\ntransport = \"local\"\nagent = \"claude\"\n"
        ));
        let problems = harness_problems(&acp_on_built_in);
        assert!(
            problems.iter().any(|p| p.contains("[harness.acp]")),
            "{problems:?}"
        );
    }

    #[test]
    fn an_unknown_harness_kind_is_rejected_without_confusing_follow_on_problems() {
        let manifest = parse(&format!(
            "{BASE}\n[[harness]]\nid = \"a\"\nkind = \"telepathy\"\ndefault = true\n"
        ));
        let problems = harness_problems(&manifest);
        assert_eq!(
            problems.len(),
            1,
            "one problem, not a cascade: {problems:?}"
        );
        assert!(problems[0].contains("telepathy") && problems[0].contains("built_in"));
    }

    /// Each ACP transport requires its own addressing field and forbids the
    /// other's, so a manifest cannot claim to spawn a local agent *and* name a
    /// remote runner.
    #[test]
    fn acp_transports_require_their_own_addressing_field() {
        let cases: &[(&str, &str)] = &[
            ("transport = \"local\"\n", "names no `agent`"),
            (
                "transport = \"local\"\nagent = \"claude\"\nrunner = \"laptop\"\n",
                "but names a `runner`",
            ),
            ("transport = \"runner\"\n", "names no `runner`"),
            (
                "transport = \"runner\"\nrunner = \"laptop\"\nagent = \"claude\"\n",
                "but names an `agent`",
            ),
            ("transport = \"carrier_pigeon\"\n", "must be one of"),
            (
                "transport = \"local\"\nagent = \"emacs\"\n",
                "must be one of",
            ),
        ];
        for (acp, expected) in cases {
            let manifest = parse(&format!(
                "{BASE}\n[[harness]]\nid = \"a\"\nkind = \"acp\"\ndefault = true\n\n[harness.acp]\n{acp}"
            ));
            let problems = harness_problems(&manifest);
            assert!(
                problems.iter().any(|p| p.contains(expected)),
                "`{acp}` should report {expected:?}, got {problems:?}"
            );
        }
    }

    #[test]
    fn a_valid_acp_harness_of_each_transport_passes() {
        for acp in [
            "transport = \"local\"\nagent = \"claude\"\n",
            "transport = \"runner\"\nrunner = \"stevens_laptop\"\n",
        ] {
            let manifest = parse(&format!(
                "{BASE}\n[[harness]]\nid = \"a\"\nkind = \"acp\"\ndefault = true\n\n[harness.acp]\n{acp}"
            ));
            assert!(
                harness_problems(&manifest).is_empty(),
                "`{acp}` should be valid: {:?}",
                harness_problems(&manifest)
            );
        }
    }

    /// Issue #1245: `model` is a hint forwarded to the agent's own startup
    /// lever, not a credential — so unlike `[harness.inference]` it is
    /// perfectly valid on a `local` acp harness. It is rejected on `runner`
    /// (no wire protocol yet) and when set to an empty string (nothing to
    /// forward, and silently accepting it invites "my model setting does
    /// nothing").
    #[test]
    fn model_is_valid_on_local_rejected_on_runner_and_must_not_be_empty() {
        let cases: &[(&str, Option<&str>)] = &[
            (
                "transport = \"local\"\nagent = \"claude\"\nmodel = \"claude-opus-4-5\"\n",
                None,
            ),
            (
                "transport = \"runner\"\nrunner = \"laptop\"\nmodel = \"claude-opus-4-5\"\n",
                Some("but names a `model`"),
            ),
            (
                "transport = \"local\"\nagent = \"claude\"\nmodel = \"   \"\n",
                Some("is set but empty"),
            ),
        ];
        for (acp, expected) in cases {
            let manifest = parse(&format!(
                "{BASE}\n[[harness]]\nid = \"a\"\nkind = \"acp\"\ndefault = true\n\n[harness.acp]\n{acp}"
            ));
            let problems = harness_problems(&manifest);
            match expected {
                None => assert!(problems.is_empty(), "`{acp}` should be valid: {problems:?}"),
                Some(msg) => assert!(
                    problems.iter().any(|p| p.contains(msg)),
                    "`{acp}` should report {msg:?}, got {problems:?}"
                ),
            }
        }
    }

    /// Issue #1245's per-agent follow-up: `agent.model` follows the exact
    /// same doctrine as `[harness.acp].model` — valid on `local`, rejected on
    /// `runner`, rejected when empty — plus one rule the harness-level field
    /// has no need for: it is meaningless on a `built_in` harness, since
    /// there is no ACP session to steer a model through.
    #[test]
    fn agent_model_follows_the_harness_level_models_own_doctrine() {
        let cases: &[(&str, &str, Option<&str>)] = &[
            (
                "kind = \"built_in\"\ndefault = true",
                "model = \"opus-4-5\"",
                Some("has no ACP transport to forward it to"),
            ),
            (
                "kind = \"acp\"\ndefault = true\n\n[harness.acp]\ntransport = \"local\"\nagent = \"claude\"",
                "model = \"opus-4-5\"",
                None,
            ),
            (
                "kind = \"acp\"\ndefault = true\n\n[harness.acp]\ntransport = \"runner\"\nrunner = \"laptop\"",
                "model = \"opus-4-5\"",
                Some("uses `transport = \"runner\"`"),
            ),
            (
                "kind = \"acp\"\ndefault = true\n\n[harness.acp]\ntransport = \"local\"\nagent = \"claude\"",
                "model = \"   \"",
                Some("is set but empty"),
            ),
        ];
        for (harness, agent_model, expected) in cases {
            let manifest = parse(&format!(
                "[company]\nname = \"X\"\n\n[[agent]]\nid = \"ceo\"\nrole = \"CEO\"\n{agent_model}\n\n\
                 [[harness]]\nid = \"a\"\n{harness}\n"
            ));
            let problems = manifest.validate();
            match expected {
                None => assert!(
                    !problems.iter().any(|p| p.contains("model")),
                    "{harness} / {agent_model} should be valid: {problems:?}"
                ),
                Some(msg) => assert!(
                    problems.iter().any(|p| p.contains(*msg)),
                    "{harness} / {agent_model} should report {msg:?}, got {problems:?}"
                ),
            }
        }
    }

    #[test]
    fn an_acp_harness_with_no_acp_section_is_rejected() {
        let manifest = parse(&format!(
            "{BASE}\n[[harness]]\nid = \"a\"\nkind = \"acp\"\ndefault = true\n"
        ));
        let problems = harness_problems(&manifest);
        assert!(
            problems.iter().any(|p| p.contains("needs a `transport`")),
            "{problems:?}"
        );
    }

    #[test]
    fn harness_ids_must_be_snake_case() {
        let manifest = parse(&format!(
            "{BASE}\n[[harness]]\nid = \"My Harness\"\nkind = \"built_in\"\ndefault = true\n"
        ));
        let problems = harness_problems(&manifest);
        assert!(
            problems.iter().any(|p| p.contains("snake_case")),
            "{problems:?}"
        );
    }

    /// The per-file roster form carries `harness` through, so the two authoring
    /// forms agree.
    #[test]
    fn a_per_file_agent_carries_its_harness_binding() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join(MANIFEST_FILE),
            "[company]\nname = \"X\"\n\n[[harness]]\nid = \"embedded\"\nkind = \"built_in\"\ndefault = true\n\n[[harness]]\nid = \"deep\"\nkind = \"built_in\"\n",
        )
        .expect("write manifest");
        let agents = dir.path().join(super::super::agent_file::AGENTS_DIR);
        std::fs::create_dir_all(&agents).expect("agents dir");
        std::fs::write(
            agents.join("researcher.toml"),
            "role = \"Researcher\"\nharness = \"deep\"\n",
        )
        .expect("write agent");

        let manifest = CompanyManifest::from_path(dir.path()).expect("parses");
        assert_eq!(
            manifest.harness_for("researcher").map(|h| h.id),
            Some("deep".to_string())
        );
    }
}
