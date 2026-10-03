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

        if let Some(problem) = legacy_hive_block(&text).or_else(|| legacy_speech_block(&text)) {
            return Err(OpenCompanyError::ManifestParse(path.to_path_buf(), problem));
        }
        toml::from_str(&text).map_err(|err| {
            OpenCompanyError::ManifestParse(path.to_path_buf(), err.message().to_string())
        })
    }

    /// Whether a manifest still carries the retired `[group_chat.hive]`
    /// block, and the migration hint if it does.
    ///
    /// The block is refused rather than ignored: `GroupChat` no longer has a
    /// field for it, so a plain `toml::from_str` would drop it silently and a
    /// desk an operator tuned by hand would run on the defaults with nothing
    /// saying so. Read off the raw document because the typed manifest cannot
    /// see a key it does not declare.
    pub fn legacy_hive_block(text: &str) -> Option<String> {
        legacy_hive_block(text)
    }

    /// Whether a manifest still carries the retired `[speech]` block, and the
    /// migration hint if it does (plan hive-desks, Phase 6).
    ///
    /// Speaking is no longer a belt tool a company opts into: every agent is
    /// served `post`, `broadcast`, `dm` and `complete_episode` by the
    /// `opencompany` MCP server, so the block has nothing left to switch.
    pub fn legacy_speech_block(text: &str) -> Option<String> {
        legacy_speech_block(text)
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

    /// The company's own teammates, without the baseline.
    ///
    /// [`apply_globals`](Self::apply_globals) appends the host's baseline to
    /// every roster on every load, so `agents` is the company's roster plus
    /// four teammates it did not add and cannot remove. Anything answering
    /// *how many teammates has this company got* means this, not that.
    pub fn own_agents(&self) -> impl Iterator<Item = &crate::company::Agent> {
        self.agents.iter().filter(|agent| !agent.global)
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

        if enforce_reserved_agent_ids && self.company.general_desk.is_some() {
            problems.push(
                "`[company].general_desk` is no longer supported — #general is built in and every teammate is in it. Remove the key; the desk it named stays an ordinary desk.".into(),
            );
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
                && (crate::ports::general_channel::is_general_spelling(&chat.id)
                    || crate::ports::general_channel::is_general_spelling(&chat.name))
            {
                problems.push(format!(
                    "{label} uses the id or name of #general, the built-in company-wide channel — choose a different id and name."
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

            problems.extend(chat.hive.problems(&label));
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
        problems.extend(self.validate_agent_pairs());

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

        problems
    }

    /// Validates each agent's `model` (and, on a `built_in` harness, `provider`)
    /// against the harness it is bound to (keys rework slice 3a, issue #2306).
    ///
    /// Deliberately **not** nested inside [`validate_harnesses`](Self::validate_harnesses):
    /// that function returns early when the manifest declares no `[[harness]]`
    /// at all (the implicit-`built_in`-default case, which is what every
    /// shipped company without an explicit block has), and this check must
    /// still run for that case — [`harness_for`](Self::harness_for) already
    /// resolves it to the synthesized implicit harness, so calling it directly
    /// here rather than living downstream of that early return is what makes
    /// the pair rule apply to the common manifest instead of only to one that
    /// bothers to declare `[[harness]]`.
    ///
    /// - On an `acp` harness: `model` follows the pre-existing per-agent
    ///   doctrine (issue #1245) unchanged — valid, forwarded to the agent's own
    ///   session, except on a `runner` transport, which cannot carry it.
    ///   `provider` is refused outright: an ACP agent brings its own credential.
    /// - On a `built_in` harness: `provider` and `model` are a pair — both set
    ///   or neither. Neither means "follow the company default". `provider` is
    ///   checked for slug shape only (`store::slugify`/`MAX_PROVIDER_NAME_CHARS`);
    ///   `model` through the shared [`check_model_id`](super::inference::store::check_model_id).
    ///   Never checked against the company's actual provider list — that list
    ///   is console data a manifest cannot see, so a slug nothing has yet
    ///   fails the agent's first turn (F6) rather than at load.
    fn validate_agent_pairs(&self) -> Vec<String> {
        use super::inference::store;

        let mut problems = Vec::new();

        for agent in &self.agents {
            let provider = agent.provider.as_deref();
            let model = agent.model.as_deref();
            if provider.is_none() && model.is_none() {
                continue;
            }

            if provider.is_some_and(|p| p.trim().is_empty()) {
                problems.push(format!(
                    "agent `{}`'s `provider` is set but empty. Drop the key to use the \
                     company default provider and model.",
                    agent.id
                ));
                continue;
            }
            if model.is_some_and(|m| m.trim().is_empty()) {
                problems.push(format!(
                    "agent `{}`'s `model` is set but empty. Drop the key to use the harness's \
                     own default, rather than naming an empty one.",
                    agent.id
                ));
                continue;
            }

            // Skipped when the agent names an unknown harness: `validate_harnesses`
            // already reports that (it runs whether or not `[[harness]]` is
            // declared), and piling a second, confusing complaint about the
            // pair on top would not help. `harness_for` is total for every
            // other manifest shape (falls back to the default, or to the
            // implicit local ACP harness), so `None` here means exactly that.
            let Some(harness) = self.harness_for(&agent.id) else {
                continue;
            };

            if harness.kind == "acp" {
                if provider.is_some() {
                    problems.push(format!(
                        "agent `{}` names a `provider` but runs on harness `{}` (`kind = \"acp\"`), \
                         which brings its own provider. Drop `provider`, or bind a `built_in` harness.",
                        agent.id, harness.id
                    ));
                }
                if model.is_some()
                    && harness.acp.as_ref().map(|a| a.transport.as_str()) == Some("runner")
                {
                    problems.push(format!(
                        "agent `{}` names a `model` but its harness `{}` uses \
                         `transport = \"runner\"`. Model overrides aren't supported for a \
                         runner yet — the runner wire protocol doesn't carry them.",
                        agent.id, harness.id
                    ));
                }
                continue;
            }

            match (provider, model) {
                (Some(p), Some(m)) => {
                    if store::slugify(p) != p || p.chars().count() > store::MAX_PROVIDER_NAME_CHARS
                    {
                        problems.push(format!(
                            "agent `{}`'s `provider` `{p}` is not a provider slug: lowercase \
                             letters, digits and `-`, at most {} characters.",
                            agent.id,
                            store::MAX_PROVIDER_NAME_CHARS
                        ));
                    }
                    if let Err(why) = store::check_model_id(m) {
                        problems.push(format!("agent `{}`'s `model`: {why}", agent.id));
                    }
                }
                (None, Some(_)) => problems.push(format!(
                    "agent `{}` names a `model` but no `provider`, on harness `{}` \
                     (`kind = \"{}\"`). Set `provider` too, or bind an `acp` harness.",
                    agent.id, harness.id, harness.kind
                )),
                (Some(_), None) => problems.push(format!(
                    "agent `{}` names a `provider` but no `model`. Set `model` too, or drop \
                     `provider` to use the company default.",
                    agent.id
                )),
                (None, None) => unreachable!("both-absent returned above"),
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
        // `local:` schemes sharing this column. An entry that normalizes to
        // `local:owner` — `normalize_email` only lowercases and trims — would
        // be stored under the `none`-mode local owner's own key and misparse
        // as that identity rather than the email admin it was meant to be.
        // Caught here so it never reaches a running company. An `@` is not
        // demanded: a login on a host with no mail is a username.
        for admin in &self.users.admins {
            if !crate::ports::users::is_usable_admin_email(admin) {
                problems.push(format!(
                    "`[users].admins` has an invalid entry: `{admin}` is not a usable login"
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
#[path = "manifest_tests_general.rs"]
mod tests_general;
#[cfg(test)]
#[path = "manifest_tests_grants.rs"]
mod tests_grants;
#[cfg(test)]
#[path = "manifest_tests_roster.rs"]
mod tests_roster;
#[cfg(test)]
#[path = "manifest_tests_surfaces.rs"]
mod tests_surfaces;

#[cfg(test)]
#[path = "manifest_harness_tests.rs"]
mod harness_tests;

/// The migration hint for a manifest that still declares `[speech]` (plan
/// hive-desks, Phase 6).
fn legacy_speech_block(text: &str) -> Option<String> {
    let document: toml::Value = toml::from_str(text).ok()?;
    document.get("speech")?;
    Some(
        "`[speech]` no longer exists — speaking is not a belt tool a company switches on: every \
         agent is served `post`, `broadcast`, `dm`, `complete_episode` and `read` by the \
         `opencompany` MCP server (`docs/spec/runtime/hive.md`). Delete the block."
            .to_string(),
    )
}

/// The migration hint for a manifest that still declares `[group_chat.hive]`
/// (plan hive-desks, Phase 4).
fn legacy_hive_block(text: &str) -> Option<String> {
    let document: toml::Value = toml::from_str(text).ok()?;
    let desks = document.get("group_chat")?.as_array()?;
    let stale: Vec<String> = desks
        .iter()
        .filter(|desk| desk.get("hive").is_some())
        .map(|desk| {
            desk.get("id")
                .and_then(toml::Value::as_str)
                .unwrap_or("?")
                .to_string()
        })
        .collect();
    if stale.is_empty() {
        return None;
    }
    Some(format!(
        "group chat `{}` declares `[group_chat.hive]`, which no longer exists — the trace-grammar          hive (quorum, moves, aside, turn_budget) was replaced by completion-driven episodes.          Delete the block, and say how the desk routes and paces its rounds under          `[group_chat.routing]` (`round_width`, `max_rounds`, `turn_timeout_secs`) and          `[group_chat.routing.referral]` (`enabled`, `max_hops`, `reach`, `returns`); see          `docs/spec/runtime/hive.md`.",
        stale.join("`, `")
    ))
}
