//! Coding + web tool wiring for embedded company agents (Cell A).
//!
//! This module bridges a curated slice of OpenHuman's tool surface into the
//! harness's per-agent [`AgentBuilder`](openhuman_core::openhuman::agent::AgentBuilder)
//! wiring in [`build`](crate::harness::build). Where [`file_tools`] grants an
//! agent read/write inside its own workspace, this module adds the **exec-grade**
//! families behind their own grant namespaces:
//!
//! * **`shell`** → `shell` (run commands) + `read_workspace_state`.
//! * **`code`** → `apply_patch`, `git_operations`, `csv_export`.
//! * **`web`** → `web_fetch`, `http_request`, `curl`, `image_info`.
//! * **`subagent`** → reserved, **empty in v1** (see [`subagent_tools`]).
//!
//! Everything here is scoped **per company/agent workspace** — no
//! process-global state — so parallel tenants stay isolated:
//!
//! * Shell + code tools share one [`SecurityPolicy`] built by [`exec_security`],
//!   pinned to the agent's workspace with `workspace_only`, high-risk commands
//!   blocked, tool-install denied, and an autonomy level mapped from the
//!   company's [`PolicyMode`] by [`autonomy_for`] — no longer 1:1 since `auto`
//!   (issue #560) has no upstream counterpart and borrows `Supervised`. This is
//!   the *strict* policy — opencompany's own
//!   [`ApprovalPolicy`](crate::harness::policy::ApprovalPolicy) tool policy stays
//!   the real fail-closed approval gate on top of it, **except** on the workflow
//!   `tool_call` path, where no such gate is installed and this policy is the
//!   whole tier. See [`autonomy_for`].
//! * Shell command audit is keyed on a per-agent, **host-owned** sink directory
//!   ([`shell_audit`]) — `companies/<slug>/audit/<agent>/`, deliberately outside
//!   the agent's own workspace — so audit trails never cross tenants *and* the
//!   agent's sanctioned write paths cannot reach the record of what it did
//!   (issue #775). The shell tool itself is wrapped by
//!   [`AuditedShellTool`](crate::harness::audit::AuditedShellTool), which
//!   appends the command's intent line *before* the command runs and refuses
//!   the call if that append fails.
//! * Web tools reuse OpenHuman's upstream SSRF guard (`url_guard`): every
//!   request is validated against the per-company allowlist AND has
//!   private/loopback/link-local/metadata IPs rejected — even in the default
//!   "allow all public hosts" mode (an empty allowlist). The guard is applied
//!   inside the tool constructors; it is not re-implemented here.
//!
//! A single [`filter_by_capabilities`] pass runs just before the tool vector is
//! handed to the builder. Today the only [`CapabilityFilter`] is
//! [`CapabilityFilter::AllowAll`] (identity); a future capability-tier cell only
//! swaps how the filter is constructed. Tools with no mapped namespace
//! (memory, MCP, orchestrator, skills) are **intrinsic** and always kept.
//!
//! **Admitted since v1** — `search` (issue #238) is no longer deferred. It was
//! held back for one *infrastructure* reason ("need engine keys"), which the
//! managed-platform-credential pattern from #109 dissolved: the backend proxies
//! the search and bills the platform, so there is no engine key to hold. It
//! lives in [`search`](crate::harness::search) rather than here because it is a
//! priced backend call rather than a local exec tool, but it is namespaced by
//! [`namespace_of`] like any other gateable family.
//!
//! **Still deferred** (need infrastructure not present in v1): browser
//! automation (needs a backend), Node/NPM exec (need a managed-runtime
//! bootstrap), and OpenHuman's sub-agent spawn tools (global registry + budget
//! bypass — unsafe under multi-tenancy). Those three were excluded for *safety*
//! reasons that still hold, which is exactly why search could move and they
//! cannot.
//!
//! **Contract — the dispatched company agent is a constrained, metered
//! derivative of an OpenHuman agent** (pinned by the contract tests in
//! [`build`](crate::harness::build)). A dispatched desk/roster agent receives
//! the curated exec subset above (`shell` / `code` / `web`) plus its intrinsic
//! memory / file / MCP / skill tools — and **nothing more**. Two invariants
//! hold for every dispatched agent and are locked by test so a future change
//! cannot silently widen or narrow the belt:
//!
//! * **Depth cap = 1 — no re-delegation.** The orchestrator's delegation tools
//!   (`query_company` / `spawn_task` / `delegate_to_desk`, plus the other
//!   orchestrator-only roster/workflow tools) are wired ONLY onto the company
//!   orchestrator; a dispatched agent never receives them, so a dispatched turn
//!   cannot fan work out further (the "no sub-agent re-delegation in v1"
//!   invariant, issue #178).
//! * **Deferred surfaces stay absent.** Raw browser automation, Node/NPM exec,
//!   OpenHuman sub-agent spawn tools (the `subagent` namespace is reserved but
//!   EMPTY in v1), skill *execution*, the raw memory-tree tool surface, and
//!   `forget` are all out of a dispatched belt. `web_search` (#238) is now
//!   admitted, but only under an **explicit** `search` grant plus a managed
//!   credential — so it is still absent from the `*`-granted belt the contract
//!   test pins.

use std::path::Path;
use std::sync::Arc;

use openhuman_core::openhuman as oh;

use oh::agent::host_runtime::{NativeRuntime, RuntimeAdapter};
use oh::config::{AuditConfig, HttpRequestConfig};
use oh::security::{
    AuditLogger, AutonomyLevel, SecurityPolicy, get_or_create_workspace_audit_logger,
};
use oh::tools::{
    ApplyPatchTool, CsvExportTool, CurlTool, GitOperationsTool, HttpRequestTool, ImageInfoTool,
    ShellTool, Tool, WebFetchTool, WorkspaceStateTool,
};

use crate::harness::policy::PolicyMode;

/// Subdirectory under the agent workspace that `curl` downloads land in.
const CURL_DEST_SUBDIR: &str = "downloads";

/// Every grant namespace a [`CapabilityFilter`] can gate — "which tool families
/// are budgeted".
///
/// This is the exec-grade surface [`namespace_of`] maps tools onto (`shell`,
/// `code`, `web`) plus the reserved `subagent` namespace. A capability plan's
/// budget map keys are validated against this set (a key outside it is a
/// manifest error), and it is the universe the fail-closed
/// [`capability_budget`](crate::harness::capability_budget) denies from when no
/// meter is available. Intrinsic tools (namespace `None`) are never listed here
/// — they are always kept regardless of the filter.
///
/// The canonical list lives in [`crate::company::GATEABLE_NAMESPACES`] (always
/// compiled, so manifest validation can see it in the default build); this is a
/// re-export for the harness call sites that key off it.
pub const GATEABLE_NAMESPACES: [&str; 7] = crate::company::GATEABLE_NAMESPACES;

/// Map a tool's runtime `name()` onto its grant namespace, or `None` when the
/// tool is **intrinsic** (memory / MCP / orchestrator / file / skill tools),
/// which are always kept regardless of the capability filter.
///
/// This is the single source of truth coupling a wired tool to the namespace
/// that gates it — [`filter_by_capabilities`] and any future capability-tier
/// logic key off it, so a new exec tool is added here and nowhere else.
pub fn namespace_of(tool_name: &str) -> Option<&'static str> {
    match tool_name {
        "shell" | "read_workspace_state" => Some("shell"),
        "apply_patch" | "git_operations" | "csv_export" => Some("code"),
        "web_fetch" | "http_request" | "curl" | "image_info" => Some("web"),
        // Media generation (issue #109). Mapped unconditionally — the arm is
        // inert without the `media` feature (the tools are never built), but the
        // namespace mapping is a pure string match so the capability filter and
        // the gateable-coverage invariant see `media` in every build.
        "media_generate_image" | "media_generate_video" | "media_list_models" => Some("media"),
        // Per-tenant Composio (issue #110). Mapped unconditionally for the same
        // reason as `media`: the arm is inert without the `composio` feature (the
        // tools are never built), but the namespace mapping is a pure string
        // match so the capability filter and the gateable-coverage invariant see
        // `composio` in every build.
        "composio_list_toolkits"
        | "composio_list_connections"
        | "composio_list_tools"
        | "composio_authorize"
        | "composio_execute" => Some("composio"),
        // Metered web search (issue #238). Lives in
        // [`search`](crate::harness::search) rather than this module because it
        // is a priced backend call, not an exec-grade local tool — but it is
        // namespaced here for the same reason `media` and `composio` are: the
        // capability filter and the gateable-coverage invariant must see
        // `search` in every build. Unlike those two the arm is never inert;
        // `web_search` compiles under the plain `openhuman` feature, which is
        // what CI actually builds and tests.
        "web_search" => Some("search"),
        // The same namespace under a company's OWN provider (the BYO half of
        // #238). `web_search` above is the canonical slot whichever provider
        // serves it — these are the provider extras that ride beside it. Mapped
        // here for the same reason: a company that budgets `search` must budget
        // every search tool, not only the metered one, or a capability ceiling
        // set on one provider evaporates when the operator switches to another.
        "exa_find_similar" | "exa_get_contents" | "brave_news_search" | "brave_image_search"
        | "brave_video_search" => Some("search"),
        _ => None,
    }
}

/// Whether `tool` is one of the raw HTTP web tools that take a plain `url`
/// argument — `web_fetch`, `http_request`, `curl`.
///
/// This is the `url`-taking subset of the `web` namespace: `image_info` is also
/// `web` but inspects a workspace file, so it is excluded. Kept here — beside
/// [`namespace_of`], the single source of truth for the family — so the S2
/// Composio deflection guardrail
/// ([`web_call_deflection`](crate::harness::composio_catalog::web_call_deflection),
/// consulted by
/// [`ApprovalPolicy::check`](crate::harness::policy::ApprovalPolicy)) recognises
/// the deflectable tools from one place rather than re-hardcoding the three
/// names where it hooks in.
pub fn is_web_request_tool(tool: &str) -> bool {
    matches!(tool, "web_fetch" | "http_request" | "curl")
}

/// Build the exec-grade [`SecurityPolicy`] shared by an agent's shell + code +
/// web tools, sandboxed to `workspace`.
///
/// Extends the same `workspace_only` shape [`file_tools`](crate::harness::build)
/// uses with the exec-relevant knobs:
///
/// * `autonomy` is mapped from the company [`PolicyMode`] — see
///   [`autonomy_for`], which is **not** 1:1 since `auto` (issue #560) has no
///   upstream counterpart, and which is a real security boundary on the
///   workflow `tool_call` path rather than a mapping detail.
/// * `block_high_risk_commands` is always on — destructive shell commands are
///   refused before they spawn.
/// * `require_approval_for_medium_risk` covers Supervised **and** Auto, for the
///   reason argued on [`autonomy_for`]: the flag is inert unless `autonomy` is
///   `Supervised`, so leaving `auto` out of it would silently undo the very
///   mapping chosen to keep `auto` from loosening shell execution.
/// * `allow_tool_install` and `auto_approve_all` are always off — an embedded
///   company agent never installs OS packages nor blanket-approves itself.
///
/// This policy is *advisory-strict*: opencompany's own
/// [`ApprovalPolicy`](crate::harness::policy::ApprovalPolicy) tool policy is the
/// authoritative park/deny gate layered above it (unlike the MCP bridge, which
/// passes a permissive OpenHuman policy).
pub fn exec_security(workspace: &Path, mode: PolicyMode) -> SecurityPolicy {
    let dir = workspace.to_path_buf();
    SecurityPolicy {
        autonomy: autonomy_for(mode),
        workspace_dir: dir.clone(),
        action_dir: dir,
        workspace_only: true,
        block_high_risk_commands: true,
        require_approval_for_medium_risk: matches!(mode, PolicyMode::Supervised | PolicyMode::Auto),
        allow_tool_install: false,
        auto_approve_all: false,
        ..SecurityPolicy::default()
    }
}

/// The OpenHuman [`AutonomyLevel`] a company [`PolicyMode`] maps to.
///
/// Three of the four map by name. `auto` (issue #560) has no upstream
/// counterpart — OpenHuman's `AutonomyLevel` is `ReadOnly` / `Supervised` /
/// `Full` — so it must borrow one, and **the borrowed level is a security
/// decision, not a naming one.**
///
/// # Why `Supervised` and not `Full`
///
/// `auto` is more permissive than `supervised` at opencompany's own
/// [`ApprovalPolicy`](crate::harness::policy::ApprovalPolicy), which is where
/// the tier is supposed to be expressed. Reaching for the matching *feel* here
/// and mapping it to `Full` would loosen a different gate in the opposite
/// direction, because this policy is not always sitting underneath that one:
///
/// * A workflow `tool_call` node runs through
///   [`WorkflowToolInvoker`](crate::workflows::caps), which gates on the
///   `[tools].allow` grant list and **this policy** — no `ApprovalPolicy` is
///   installed on that path. (Workflow *agent* nodes go through the roster and
///   do have one.) So for those nodes this is the whole tier.
/// * Upstream, `AutonomyLevel::Full` stops asking about medium-risk shell
///   commands: the approval arm in `command_checks` fires only when `autonomy
///   == Supervised`. Mapping `auto` to `Full` would therefore let medium-risk
///   commands run unapproved in workflow nodes on an `auto` company — while the
///   tier's stated contract is that `shell` parks. The inverse of what the
///   operator selected.
///
/// Mapping to `Supervised` costs nothing in the other direction. Every tool
/// this policy governs — `shell`, the code runners, the web tools — is
/// `Standing::PerCall` and therefore parks under `auto` at the authoritative
/// layer anyway, so the stricter advisory tier underneath is never the thing
/// the operator notices. `auto` buys its autonomy in tools this policy does not
/// govern.
///
/// This is also why `require_approval_for_medium_risk` in [`exec_security`]
/// lists `Auto` explicitly instead of leaving `mode == PolicyMode::Supervised`
/// to answer it. That expression was exhaustive by accident and would have
/// quietly returned `false` for the new variant — pairing `Supervised` autonomy
/// with the medium-risk gate switched off, which is the loosening this mapping
/// was chosen to prevent, arriving through the back door.
fn autonomy_for(mode: PolicyMode) -> AutonomyLevel {
    match mode {
        PolicyMode::Readonly => AutonomyLevel::ReadOnly,
        PolicyMode::Supervised | PolicyMode::Auto => AutonomyLevel::Supervised,
        PolicyMode::Full => AutonomyLevel::Full,
    }
}

/// A native (host-process) [`RuntimeAdapter`] for the shell tool. Stateless and
/// cheap — a fresh handle per agent keeps tenants from sharing runtime state.
pub fn native_runtime() -> Arc<dyn RuntimeAdapter> {
    Arc::new(NativeRuntime::new())
}

/// A shell audit logger paired with the file it appends to.
///
/// The pairing is structural on purpose. [`AuditLogger`] does not expose its own
/// path, and the fail-closed refusal in
/// [`AuditedShellTool`](crate::harness::audit::AuditedShellTool) has to *name*
/// the sink it could not write — an operator staring at a shell outage needs
/// that path. Carrying the two together means the name can never describe a
/// different file than the one being appended to.
#[derive(Clone)]
pub struct ShellAudit {
    /// The shared per-agent logger. Cloning shares one instance, so every
    /// append serializes through its write lock.
    pub logger: Arc<AuditLogger>,
    /// The file `logger` appends to, derived from the same
    /// [`AuditConfig::default`] the logger was built with.
    pub sink: std::path::PathBuf,
}

impl ShellAudit {
    /// A disabled logger over a sentinel `sink`, for tests and contexts that
    /// need a handle but must not touch the filesystem. `log()` short-circuits
    /// before any I/O, so the sink path is never opened.
    pub fn disabled() -> Self {
        Self {
            logger: AuditLogger::disabled(),
            sink: std::path::PathBuf::new(),
        }
    }
}

impl std::fmt::Debug for ShellAudit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ShellAudit")
            .field("sink", &self.sink)
            .finish_non_exhaustive()
    }
}

/// The per-agent [`AuditLogger`] for shell command execution, built the way
/// OpenHuman's `runtime_node::build_runtime_tools` does, but keyed on a
/// **host-owned** sink directory rather than the agent's workspace.
///
/// `audit_dir` is
/// [`DataLayout::agent_audit_dir`](crate::store::DataLayout::agent_audit_dir) —
/// `companies/<slug>/audit/<agent>/`, per agent and outside every agent
/// workspace. Two properties depend on that, and neither survives putting the
/// sink back in the workspace:
///
/// * The workspace is also the `workspace_only` [`SecurityPolicy`] root the
///   file tools enforce, so a sink inside it is a **policy-permitted** target:
///   the plain relative `file_write("audit.log")` — no traversal, no absolute
///   path, no `shell` — is exactly what the policy allows, and it used to land
///   on the audit trail. Outside the workspace that same call reaches nothing
///   that matters (issue #775). Absolute paths and `../` are refused either
///   way, by `workspace_only`'s own rules; they are not what moved.
/// * The vendored registry caches one logger per *directory*, first config
///   wins, so a directory shared between agents would hand the second agent the
///   first agent's file.
///
/// The directory is created here, before the logger is built: the vendored
/// factory keys its process-global registry on the *canonicalized* path and
/// silently falls back to the raw one when the directory is missing, which
/// registers one physical sink twice and reopens the interleaving race the
/// registry exists to prevent.
///
/// Returns `None` when the directory cannot be created or the logger cannot be
/// initialized. Callers **must** treat `None` as fail-closed: [`shell_tools`]
/// withholds the `ShellTool` entirely rather than register it unaudited (see
/// there). The failure is logged at `error!` (not `warn!`): losing the audit
/// logger drops shell capability for the agent, so the event must surface in
/// production error telemetry.
///
/// This makes the sink unreachable *through the sanctioned tool paths*. It is
/// not tamper-evidence — the shell runs as the same uid and can still delete the
/// file. See `docs/spec/security/agent-isolation.md`.
pub fn shell_audit(audit_dir: &Path) -> Option<ShellAudit> {
    if let Err(error) = std::fs::create_dir_all(audit_dir) {
        tracing::error!(
            audit_dir = %audit_dir.display(),
            %error,
            "[toolbelt] shell audit sink directory could not be created; withholding shell capability (fail-closed) — this agent gets NO shell tool"
        );
        return None;
    }
    let config = AuditConfig::default();
    let sink = audit_dir.join(&config.log_path);
    match get_or_create_workspace_audit_logger(config, audit_dir.to_path_buf()) {
        Ok(logger) => Some(ShellAudit { logger, sink }),
        Err(error) => {
            tracing::error!(
                audit_dir = %audit_dir.display(),
                %error,
                "[toolbelt] shell audit logger init failed; withholding shell capability (fail-closed) — this agent gets NO shell tool"
            );
            None
        }
    }
}

/// The `shell` namespace tools, sharing the exec-grade `security` policy and
/// pinned to the agent's `workspace`. Mirrors
/// [`file_tools`](crate::harness::build): a flat vector the builder extends.
///
/// Kept **disjoint** from [`code_tools`] so the builder can gate each grant
/// namespace independently — a company granting only `code` must never receive
/// a live [`ShellTool`], and vice versa (the production [`CapabilityFilter`] is
/// `AllowAll`/identity and does not re-trim namespaces post-construction).
///
/// * `shell` — run commands (needs `runtime` + `audit`; plain constructor, no
///   Node/Python bootstrap in v1).
/// * `read_workspace_state` — read-only git/tree overview.
///
/// **Fail closed on audit:** `audit` is `None` only when the per-agent audit
/// logger could not be initialized (see [`shell_audit`]). In that case the whole
/// `shell` namespace is withheld — an empty vector — so a `ShellTool` can never
/// run commands with no audit record. Dropping the capability is the safe
/// failure mode; registering an unaudited shell is not.
///
/// **Fail closed at run time too:** the `ShellTool` is wrapped in an
/// [`AuditedShellTool`](crate::harness::audit::AuditedShellTool), which appends
/// the command's intent line *before* delegating and refuses the call when that
/// append fails. Init-time fail-closed alone was not enough — upstream's
/// post-execution `emit_audit` is warn-and-continue by design, so a sink that
/// became unwritable *after* the agent was built would let commands run with no
/// record at all.
pub fn shell_tools(
    security: Arc<SecurityPolicy>,
    runtime: Arc<dyn RuntimeAdapter>,
    audit: Option<ShellAudit>,
    workspace: &Path,
) -> Vec<Box<dyn Tool>> {
    let Some(audit) = audit else {
        return Vec::new();
    };
    vec![
        Box::new(crate::harness::audit::AuditedShellTool::new(
            ShellTool::new(security, runtime, Arc::clone(&audit.logger)),
            audit,
        )),
        Box::new(WorkspaceStateTool::new(workspace.to_path_buf())),
    ]
}

/// The sandbox brief: what the agent's own working directory is, which tools
/// reach it, and the path confinement the file/code tools enforce there.
///
/// # Why this exists
///
/// Every other granted surface on this belt names itself in the prompt —
/// [`workspace_brief`](crate::harness::workspace_tools::workspace_brief) for the
/// shared note tree, [`ledger_brief`](crate::harness::ledger_tools::ledger_brief)
/// for the company's own records,
/// [`publish_brief`](crate::harness::publish::publish_brief) for handing a file
/// over. The agent's **sandbox** did not. `publish_brief` mentioned it in
/// passing, as the place a deliverable comes from, and only when an artifact
/// store happened to be wired; it named no tool, and it said nothing at all
/// about `shell`.
///
/// The observed failure is the one issue #237's brief exists to prevent, one
/// surface over: asked to *write* something, an agent that has never been told
/// it holds `file_write` records a task about writing it instead. It is not
/// refusing — it is picking the surface it was told about. A granted tool that
/// goes unmentioned is, for prompt purposes, a tool that was never granted, and
/// the `shell` namespace was in exactly that state: wired since Cell A, named
/// nowhere.
///
/// # Why it is assembled from flags rather than written once
///
/// `files`, `shell` and `code` are three independent grant namespaces
/// ([`build_agent`](crate::harness::build::build_agent) gates each separately),
/// so a single fixed paragraph would describe tools some agents do not hold —
/// the precise mistake `publish_brief`'s own comment warns about, and one that
/// costs a turn per hallucinated call. Each clause is therefore emitted only
/// under the flag that wired its tools, and an agent holding none of the three
/// gets the empty string and no section at all.
///
/// The confinement sentence is not decoration, and it is scoped on purpose.
/// [`exec_security`] sets `workspace_only`, so the **file** and `code` tools
/// refuse an absolute path or a `../` escape by policy rather than by the
/// model's judgement; an agent that does not know this spends its turns
/// discovering it one refusal at a time. The **shell** is deliberately not
/// described that way: `action_dir` only sets the command's current directory,
/// and a same-uid command can read anywhere the server can
/// (`docs/spec/security/agent-isolation.md`). The shell clause says the
/// directory is where commands *start*, never that they cannot leave it.
pub fn sandbox_brief(files: bool, shell: bool, code: bool) -> String {
    if !files && !shell && !code {
        return String::new();
    }
    let mut brief = String::from(
        "\n\n## Your sandbox\n\
         You have a real working directory of your own — a private folder on disk, separate from \
         the company workspace (the shared note tree the `workspace_*` tools read). It is where \
         your own files live.\n\
         Every path you give these tools is relative to that directory, so write `report.md` or \
         `drafts/report.md`, never `/tmp/report.md` or `~/report.md`.\n",
    );
    if files {
        brief.push_str(
            "Read and write it with `file_read`, `file_write`, `edit`, `list`, `glob` and \
             `grep`. Subdirectories are created for you on write, and an absolute path or a \
             `../` escape is refused by these tools.\n",
        );
    }
    if shell {
        brief.push_str(
            "Run commands with `shell`. It starts in that same directory, so write a command \
             against relative paths like any other tool call. `read_workspace_state` gives you \
             a read-only overview of what is there. Every command is recorded to an audit log \
             the operator can read.\n",
        );
    }
    if code {
        brief.push_str(
            "`apply_patch` applies a structured multi-file edit, `git_operations` runs \
             status/diff/log/commit inside the directory, and `csv_export` writes a CSV into \
             `exports/`.\n",
        );
    }
    if shell {
        brief.push_str(
            "When you are asked to write, produce, build or run something, do it here — \
             actually write the file or run the command.",
        );
    } else {
        brief.push_str(
            "When you are asked to write, produce or build something, do it here — actually \
             write the file.",
        );
    }
    brief.push_str(
        " Recording a task about the work, or pasting \
         the finished text into your reply, is not the same as producing it, and leaves nothing \
         on disk for anyone to open.\n\
         A command or write that touches something consequential may be held for operator \
         approval before it runs. That is a pause, not a failure: you will be told the outcome. \
         Until you are, do not report the work as done.",
    );
    brief
}

/// The `code` namespace tools, sharing the exec-grade `security` policy and
/// pinned to the agent's `workspace`. Disjoint from [`shell_tools`] (see there
/// for why the split is a security boundary, not just cosmetics). Unlike shell,
/// these need neither a host runtime nor an audit logger.
///
/// * `apply_patch` — structured multi-edit patches.
/// * `git_operations` — status/diff/log/commit within the workspace.
/// * `csv_export` — write a CSV into the workspace's `exports/` dir.
pub fn code_tools(security: Arc<SecurityPolicy>, workspace: &Path) -> Vec<Box<dyn Tool>> {
    vec![
        Box::new(ApplyPatchTool::new(security.clone())),
        Box::new(GitOperationsTool::new(
            security.clone(),
            workspace.to_path_buf(),
        )),
        Box::new(CsvExportTool::new(security)),
    ]
}

/// The `web` tools, sharing the exec-grade `security` policy and the
/// per-company `allowed_domains` SSRF allowlist.
///
/// `allowed_domains` semantics (OpenHuman's `url_guard`, applied inside each
/// constructor): an **empty** list is *open mode* — all public hosts allowed —
/// while private/loopback/link-local/multicast/metadata IPs are **always**
/// rejected regardless. A non-empty list is strict (only those hosts +
/// subdomains); `"*"` is an explicit allow-all-public wildcard.
///
/// * `web_fetch` — fetch + return page text (size/timeout defaults).
/// * `http_request` — arbitrary HTTP method/headers/body.
/// * `curl` — download a URL into the workspace `downloads/` dir.
/// * `image_info` — inspect a workspace image's dimensions/format.
pub fn web_tools(
    security: Arc<SecurityPolicy>,
    allowed_domains: Vec<String>,
    workspace: &Path,
) -> Vec<Box<dyn Tool>> {
    // Source size/timeout defaults from OpenHuman's own config so there is one
    // source of truth (and no `0 → coerced-with-warning` noise on each build).
    let http_defaults = HttpRequestConfig::default();
    vec![
        Box::new(WebFetchTool::new(
            security.clone(),
            allowed_domains.clone(),
            None,
            None,
        )),
        Box::new(HttpRequestTool::new(
            security.clone(),
            allowed_domains.clone(),
            http_defaults.max_response_size,
            http_defaults.timeout_secs,
        )),
        Box::new(CurlTool::new(
            security.clone(),
            allowed_domains,
            workspace.to_path_buf(),
            CURL_DEST_SUBDIR.to_string(),
            http_defaults.max_response_size as u64,
            http_defaults.timeout_secs,
        )),
        Box::new(ImageInfoTool::new(security)),
    ]
}

/// The managed platform credential for the media-generation backend (issue
/// #109) — the OpenHuman backend URL + the platform's own bearer token.
///
/// **Security invariant**: this holds ONLY the managed platform credential,
/// never a tenant BYOK key. The tenant identity that the backend bills is
/// derived server-side from this managed credential, so a company can never
/// point media generation at a key it controls. Threaded onto
/// [`HarnessDeps`](crate::harness::HarnessDeps) and consumed by [`media_tools`].
///
/// Always compiled (so the deps field exists in every `openhuman` build and
/// every construction site fails closed with `None`); the live tool
/// constructors in [`media_tools`] are gated behind the `media` feature.
#[derive(Clone)]
pub struct MediaBackend {
    /// The media-generation backend base URL (e.g. `https://api.tinyhumans.ai`).
    pub backend_url: String,
    /// The managed platform bearer credential. Never a tenant key.
    pub auth_token: String,
}

impl std::fmt::Debug for MediaBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never let the managed credential land in a trace.
        f.debug_struct("MediaBackend")
            .field("backend_url", &self.backend_url)
            .field(
                "auth_token",
                &if self.auth_token.is_empty() {
                    "<unset>"
                } else {
                    "<redacted>"
                },
            )
            .finish()
    }
}

/// The `media` namespace tools (issue #109): image + video generation plus the
/// model catalog, built over the MANAGED platform credential in `backend` with
/// generated artifacts pinned to the agent's `workspace` (the tools' persistence
/// `action_dir`, so nothing escapes the sandbox).
///
/// These spend real money — the backend charges on submit — so
/// [`build_agent`](crate::harness::build::build_agent) only calls this when the
/// company **explicitly** grants `media` (never via the `*` wildcard) AND a
/// managed credential is present; the generate tools additionally park for
/// operator approval through the [`ApprovalPolicy`](crate::harness::policy).
///
/// * `media_generate_image` / `media_generate_video` — submit → poll → persist,
///   billed by the backend.
/// * `media_list_models` — read-only catalog GET (needs no `action_dir`).
///
/// Gated on the `media` feature; enabling it necessarily enables
/// `openhuman_core/media`, so the upstream tool types are in scope.
#[cfg(feature = "media")]
pub fn media_tools(backend: &MediaBackend, workspace: &Path) -> Vec<Box<dyn Tool>> {
    use oh::integrations::IntegrationClient;
    use oh::media::generation::{
        MediaGenerateImageTool, MediaGenerateVideoTool, MediaListModelsTool,
    };

    // Fail closed on any backend that is not exactly HTTPS: the client attaches
    // the managed platform token and the backend charges real money on submit,
    // so an `http://` override would ship the credential over the wire. The
    // default (`https://api.tinyhumans.ai`) passes; a misconfigured host gets
    // no media tools at all, loudly, rather than a client that leaks.
    if url::Url::parse(&backend.backend_url)
        .map(|parsed| parsed.scheme() != "https")
        .unwrap_or(true)
    {
        tracing::warn!(
            backend_url = %backend.backend_url,
            "[toolbelt] refusing to wire the media tools: the backend URL must be https"
        );
        return Vec::new();
    }

    // The Config-free seam: `IntegrationClient::new(backend_url, auth_token)`
    // takes the managed credential directly, with no OpenHuman global `Config`.
    let client = Arc::new(IntegrationClient::new(
        backend.backend_url.clone(),
        backend.auth_token.clone(),
    ));
    let action_dir = workspace.to_path_buf();
    vec![
        Box::new(MediaGenerateImageTool::new(
            Arc::clone(&client),
            action_dir.clone(),
        )),
        Box::new(MediaGenerateVideoTool::new(Arc::clone(&client), action_dir)),
        Box::new(MediaListModelsTool::new(client)),
    ]
}

/// The `subagent` namespace — **reserved and empty in v1**.
///
/// OpenHuman's sub-agent spawn tools (`SpawnSubagentTool` et al.) reach a
/// process-global agent registry and can bypass per-agent budget accounting —
/// both unsafe under opencompany's multi-tenant isolation model. The namespace
/// is reserved so a company can grant `subagent` today without effect, and a
/// future cell can wire a tenant-safe delegation surface without a grant change.
pub fn subagent_tools() -> Vec<Box<dyn Tool>> {
    Vec::new()
}

/// Which tools an agent may keep after its grants have already admitted them —
/// the seam the capability-tier gate ([`capability_budget`](crate::harness::capability_budget))
/// constructs per tenant, per turn.
///
/// [`AllowAll`](Self::AllowAll) is the identity pass (no plan configured, or a
/// tenant under every tier's budget); [`DenyNamespaces`](Self::DenyNamespaces)
/// drops the exec families whose per-period token budget the tenant has spent
/// through — or, fail-closed, every gateable family when the meter can't be
/// read.
#[derive(Clone, Debug, Default)]
pub enum CapabilityFilter {
    /// Keep every tool the grants admitted (identity).
    #[default]
    AllowAll,
    /// Drop every tool whose [`namespace_of`] is in this set; intrinsic tools
    /// (namespace `None`) are always kept.
    DenyNamespaces(std::collections::HashSet<&'static str>),
}

/// Apply a [`CapabilityFilter`] to a built tool vector, just before it is handed
/// to the [`AgentBuilder`](openhuman_core::openhuman::agent::AgentBuilder).
///
/// Intrinsic tools (memory / MCP / orchestrator / file / skill — any tool whose
/// [`namespace_of`] is `None`) are always kept; only namespaced exec tools can
/// be dropped. [`CapabilityFilter::AllowAll`] is the identity pass.
pub fn filter_by_capabilities(
    tools: Vec<Box<dyn Tool>>,
    filter: &CapabilityFilter,
) -> Vec<Box<dyn Tool>> {
    match filter {
        CapabilityFilter::AllowAll => tools,
        CapabilityFilter::DenyNamespaces(denied) => tools
            .into_iter()
            .filter(|tool| match namespace_of(tool.name()) {
                Some(namespace) => !denied.contains(namespace),
                // Intrinsic tools have no namespace and are never dropped.
                None => true,
            })
            .collect(),
    }
}

/// The native capability namespaces actually present on a built tool belt.
///
/// Derived the same way [`filter_by_capabilities`] reads the belt — each tool's
/// [`namespace_of`] — kept to the shared native vocabulary
/// ([`native_capability_namespaces`](crate::company::native_capability_namespaces)),
/// so a tool that was wired makes its namespace show up here and one that was
/// not never does. Sorted and unique for a stable system-prompt rendering.
pub fn native_capabilities_on_belt(
    tools: &[Box<dyn Tool>],
) -> std::collections::BTreeSet<&'static str> {
    let native = crate::company::native_capability_namespaces();
    tools
        .iter()
        .filter_map(|tool| namespace_of(tool.name()))
        .filter(|ns| native.contains(ns))
        .collect()
}

/// Whether a [`CapabilityFilter`] denies a given namespace — the same test
/// [`filter_by_capabilities`] applies per tool, exposed standalone so a
/// caller that needs the outcome without a tool vector in hand (the sandbox
/// brief, built before tools are filtered) can ask it directly.
///
/// [`CapabilityFilter::AllowAll`] denies nothing; a name outside
/// [`GATEABLE_NAMESPACES`] cannot be denied by construction (`DenyNamespaces`
/// is only ever populated from that set), so this returns `false` for those
/// too rather than requiring the caller to special-case them.
pub fn namespace_denied(filter: &CapabilityFilter, namespace: &str) -> bool {
    match filter {
        CapabilityFilter::AllowAll => false,
        CapabilityFilter::DenyNamespaces(denied) => denied.contains(namespace),
    }
}

/// Whether the per-tenant Composio surface may be wired for this agent's
/// current turn — one predicate shared by both the S1 brief
/// ([`build::build_agent`](crate::harness::built_in::build::build_agent)) and
/// the S2 deflection policy
/// ([`build_roster`](crate::harness::built_in::build_roster)) so the two
/// cannot drift back out of lockstep the way they did before this fix (PR
/// #1780 review, issue #1759).
///
/// `wired` is the grant+credential outcome each call site already resolved
/// (an explicit `composio` grant AND a resolved credential with a non-empty
/// toolkit allowlist) — this function does not re-derive it, only narrows it
/// by the per-turn capability tier. When [`namespace_denied`] reports
/// `composio` denied (a `free`/`starter`/`pro` plan's Composio budget is
/// exhausted, or a fail-closed metering error), `filter_by_capabilities`
/// strips every `composio_*` tool from the belt — describing the brief or
/// installing the deflection anyway would ground the agent in a surface it no
/// longer holds, or point a blocked web call at a tool that is not on the
/// belt.
///
/// Deliberately NOT behind the `composio` feature, like the rest of this
/// module's namespace plumbing and like `composio_catalog`'s own S1/S2 pair —
/// pure logic stays outside the gate so CI's fast, always-run `openhuman`
/// lane exercises it, rather than only the `composio` feature's partial lane.
pub fn composio_capability_admits(wired: bool, capabilities: &CapabilityFilter) -> bool {
    wired && !namespace_denied(capabilities, "composio")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashSet;

    fn names(tools: &[Box<dyn Tool>]) -> Vec<&str> {
        tools.iter().map(|t| t.name()).collect()
    }

    /// The brief must name every tool the flag it rides on actually wires, or
    /// it re-creates the bug it exists to fix one namespace at a time. Each
    /// list is read off the matching constructor above rather than retyped, so
    /// a belt that grows a tool fails here instead of shipping a brief that
    /// silently omits it.
    #[test]
    fn each_flag_names_exactly_the_tools_its_constructor_wires() {
        let ws = Path::new("/tmp/agent-ws");
        let security = test_security(ws, PolicyMode::Full);

        let shell = sandbox_brief(false, true, false);
        for tool in names(&shell_tools(
            security.clone(),
            native_runtime(),
            Some(ShellAudit::disabled()),
            ws,
        )) {
            assert!(shell.contains(tool), "the shell brief never names `{tool}`");
        }

        let code = sandbox_brief(false, false, true);
        for tool in names(&code_tools(security, ws)) {
            assert!(code.contains(tool), "the code brief never names `{tool}`");
        }

        // `file_tools` lives in `build` (behind the same feature as this
        // module), so its belt is named literally here and pinned by
        // `build::file_tools_are_sandboxed_to_the_workspace` on the other side.
        let files = sandbox_brief(true, false, false);
        for tool in ["file_read", "file_write", "edit", "list", "glob", "grep"] {
            assert!(files.contains(tool), "the file brief never names `{tool}`");
        }
    }

    /// A brief that describes an ungranted namespace costs a turn per
    /// hallucinated call, so each clause must be absent when its flag is.
    #[test]
    fn a_clause_is_absent_when_its_namespace_is_not_granted() {
        let files_only = sandbox_brief(true, false, false);
        assert!(!files_only.contains("`shell`"), "{files_only}");
        assert!(!files_only.contains("apply_patch"), "{files_only}");

        let shell_only = sandbox_brief(false, true, false);
        assert!(!shell_only.contains("file_write"), "{shell_only}");
        assert!(!shell_only.contains("csv_export"), "{shell_only}");
    }

    /// An agent holding none of the three gets no section at all — not an empty
    /// heading, which would read as a surface it has and cannot find.
    #[test]
    fn an_agent_with_no_sandbox_namespace_gets_no_section() {
        assert_eq!(sandbox_brief(false, false, false), "");
    }

    /// The two things the sandbox brief exists to say, both of which the belt
    /// enforces whether or not the agent knows them: file/code paths are
    /// confined (`exec_security` sets `workspace_only`), and producing the
    /// thing means writing it rather than recording a task about it.
    #[test]
    fn the_brief_states_the_confinement_and_the_write_it_instruction() {
        let brief = sandbox_brief(true, true, true);
        assert!(
            brief.contains("../"),
            "the escape rule must be shown: {brief}"
        );
        assert!(
            brief.contains("actually write the file"),
            "the instruction that motivates this brief is missing: {brief}"
        );
        assert!(
            brief.contains("Recording a task about the work"),
            "the observed failure must be named: {brief}"
        );
    }

    /// The confinement claim must be scoped to the tools that enforce it.
    /// `workspace_only` refuses an absolute path or a `../` escape for the
    /// file/code tools, but `action_dir` only sets the shell's *working
    /// directory* — a same-uid command can read anywhere the server can
    /// (docs/spec/security/agent-isolation.md). So the shell clause must
    /// describe the directory as where commands start, never as a jail.
    #[test]
    fn the_shell_clause_does_not_claim_confinement() {
        let shell_only = sandbox_brief(false, true, false);
        assert!(!shell_only.contains("cannot leave"), "{shell_only}");
        assert!(!shell_only.contains("nothing outside"), "{shell_only}");
        assert!(
            shell_only.contains("starts in that same directory"),
            "{shell_only}"
        );

        // The refusal sentence stays with the file tools that enforce it.
        let files_only = sandbox_brief(true, false, false);
        assert!(
            files_only.contains("`../` escape is refused"),
            "{files_only}"
        );
    }

    /// "Run the command" is a command-running instruction, and the only tool
    /// that runs arbitrary commands is `shell`. A belt without `shell` must
    /// not be told to run anything — that re-creates the unavailable-tool
    /// prompt mismatch the namespace filtering exists to prevent.
    #[test]
    fn the_run_instruction_is_gated_on_shell() {
        let files_only = sandbox_brief(true, false, false);
        assert!(!files_only.contains("run the command"), "{files_only}");
        assert!(!files_only.contains("or run"), "{files_only}");

        let with_shell = sandbox_brief(true, true, false);
        assert!(with_shell.contains("run the command"), "{with_shell}");
    }

    fn test_security(workspace: &Path, mode: PolicyMode) -> Arc<SecurityPolicy> {
        Arc::new(exec_security(workspace, mode))
    }

    #[test]
    fn shell_tools_expose_expected_names() {
        let ws = Path::new("/tmp/oc-toolbelt-shell");
        let security = test_security(ws, PolicyMode::Supervised);
        let tools = shell_tools(security, native_runtime(), Some(ShellAudit::disabled()), ws);
        let got = names(&tools);
        for expected in ["shell", "read_workspace_state"] {
            assert!(got.contains(&expected), "missing {expected}: {got:?}");
        }
        assert_eq!(got.len(), 2, "shell tools drifted: {got:?}");
    }

    /// Fail-closed guard: when the workspace audit logger cannot be built
    /// (`workspace_audit` → `None`), `shell_tools` MUST withhold the entire
    /// `shell` namespace rather than register a `ShellTool` that would execute
    /// commands with no audit record. This is the security boundary — pin it.
    #[test]
    fn shell_tools_absent_when_audit_init_fails() {
        let ws = Path::new("/tmp/oc-toolbelt-shell-noaudit");
        let security = test_security(ws, PolicyMode::Supervised);
        let tools = shell_tools(security, native_runtime(), None, ws);
        assert!(
            tools.is_empty(),
            "shell namespace must be withheld when audit init fails, got: {:?}",
            names(&tools)
        );
    }

    #[test]
    fn code_tools_expose_expected_names() {
        let ws = Path::new("/tmp/oc-toolbelt-code");
        let security = test_security(ws, PolicyMode::Supervised);
        let tools = code_tools(security, ws);
        let got = names(&tools);
        for expected in ["apply_patch", "git_operations", "csv_export"] {
            assert!(got.contains(&expected), "missing {expected}: {got:?}");
        }
        assert_eq!(got.len(), 3, "code tools drifted: {got:?}");
    }

    /// The `shell` and `code` grant namespaces must build from **disjoint** tool
    /// vectors: granting `code` alone must never hand an agent a live `ShellTool`
    /// (and vice versa). The production `CapabilityFilter` is identity, so this
    /// tool-vector split is the only thing enforcing the boundary — pin it.
    #[test]
    fn shell_and_code_tool_sets_are_disjoint_and_correctly_namespaced() {
        let ws = Path::new("/tmp/oc-toolbelt-isolation");
        let security = test_security(ws, PolicyMode::Supervised);

        let shell = shell_tools(
            security.clone(),
            native_runtime(),
            Some(ShellAudit::disabled()),
            ws,
        );
        let code = code_tools(security, ws);

        // Every shell tool maps to the `shell` namespace and none to `code`.
        for tool in &shell {
            assert_eq!(
                namespace_of(tool.name()),
                Some("shell"),
                "shell_tools leaked a non-shell tool: {}",
                tool.name()
            );
        }
        // Every code tool maps to the `code` namespace and none to `shell`.
        for tool in &code {
            assert_eq!(
                namespace_of(tool.name()),
                Some("code"),
                "code_tools leaked a non-code tool: {}",
                tool.name()
            );
        }

        // No tool name appears in both vectors.
        let shell_names: HashSet<&str> = names(&shell).into_iter().collect();
        let code_names: HashSet<&str> = names(&code).into_iter().collect();
        assert!(
            shell_names.is_disjoint(&code_names),
            "shell/code tool sets overlap: {shell_names:?} ∩ {code_names:?}"
        );
    }

    #[test]
    fn web_tools_expose_expected_names() {
        let ws = Path::new("/tmp/oc-toolbelt-web");
        let security = test_security(ws, PolicyMode::Supervised);
        // Empty allowlist = open-public mode; the SSRF IP guard still applies.
        let tools = web_tools(security, Vec::new(), ws);
        let got = names(&tools);
        for expected in ["web_fetch", "http_request", "curl", "image_info"] {
            assert!(got.contains(&expected), "missing {expected}: {got:?}");
        }
        assert_eq!(got.len(), 4, "web tools drifted: {got:?}");
    }

    #[test]
    fn subagent_tools_are_reserved_empty() {
        assert!(
            subagent_tools().is_empty(),
            "subagent namespace is v1-reserved"
        );
    }

    #[test]
    fn namespace_table_maps_exec_tools_and_leaves_intrinsic_unmapped() {
        assert_eq!(namespace_of("shell"), Some("shell"));
        assert_eq!(namespace_of("read_workspace_state"), Some("shell"));
        assert_eq!(namespace_of("apply_patch"), Some("code"));
        assert_eq!(namespace_of("git_operations"), Some("code"));
        assert_eq!(namespace_of("csv_export"), Some("code"));
        assert_eq!(namespace_of("web_fetch"), Some("web"));
        assert_eq!(namespace_of("http_request"), Some("web"));
        assert_eq!(namespace_of("curl"), Some("web"));
        assert_eq!(namespace_of("image_info"), Some("web"));
        // Media generation (issue #109) maps to the `media` namespace.
        assert_eq!(namespace_of("media_generate_image"), Some("media"));
        assert_eq!(namespace_of("media_generate_video"), Some("media"));
        assert_eq!(namespace_of("media_list_models"), Some("media"));
        // Per-tenant Composio (issue #110) maps to the `composio` namespace.
        assert_eq!(namespace_of("composio_list_toolkits"), Some("composio"));
        assert_eq!(namespace_of("composio_list_connections"), Some("composio"));
        assert_eq!(namespace_of("composio_list_tools"), Some("composio"));
        assert_eq!(namespace_of("composio_authorize"), Some("composio"));
        assert_eq!(namespace_of("composio_execute"), Some("composio"));
        // Metered web search (issue #238) maps to the `search` namespace, so a
        // token-budget plan can shed it under spend pressure.
        assert_eq!(namespace_of("web_search"), Some("search"));
        // Intrinsic tools are unmapped (always kept by the filter).
        assert_eq!(namespace_of("memory_store"), None);
        assert_eq!(namespace_of("memory_recall"), None);
        assert_eq!(namespace_of("memory_forget"), None);
        assert_eq!(namespace_of("file_read"), None);
        assert_eq!(namespace_of("mcp_registry_tool_call"), None);
    }

    /// The `url`-taking web subset the S2 deflection guardrail keys on: the three
    /// raw HTTP tools, and NOT `image_info` (which is `web` but reads a workspace
    /// file, not a URL) nor anything outside the family.
    #[test]
    fn is_web_request_tool_is_the_url_taking_web_subset() {
        assert!(is_web_request_tool("web_fetch"));
        assert!(is_web_request_tool("http_request"));
        assert!(is_web_request_tool("curl"));
        assert!(!is_web_request_tool("image_info"));
        assert!(!is_web_request_tool("shell"));
        assert!(!is_web_request_tool("composio_execute"));
    }

    /// `GATEABLE_NAMESPACES` must be a superset of every namespace `namespace_of`
    /// can emit — otherwise an exec family would be ungateable (silently always
    /// granted). `subagent` is additionally present as the reserved namespace.
    #[test]
    fn gateable_namespaces_cover_every_mapped_namespace() {
        let mapped = [
            "shell",
            "read_workspace_state",
            "apply_patch",
            "git_operations",
            "csv_export",
            "web_fetch",
            "http_request",
            "curl",
            "image_info",
            "media_generate_image",
            "media_generate_video",
            "media_list_models",
            "composio_list_toolkits",
            "composio_list_connections",
            "composio_list_tools",
            "composio_authorize",
            "composio_execute",
            "web_search",
            // The BYO search extras (issue #238 follow-up). Listed by name
            // rather than spliced in from `BYO_SEARCH_TOOLS` so this test keeps
            // saying what it checks: every tool a belt can carry is mapped onto
            // a gateable namespace.
            "exa_find_similar",
            "exa_get_contents",
            "brave_news_search",
            "brave_image_search",
            "brave_video_search",
        ];
        for tool in mapped {
            let ns = namespace_of(tool).expect("mapped tool has a namespace");
            assert!(
                GATEABLE_NAMESPACES.contains(&ns),
                "namespace `{ns}` (from `{tool}`) is not gateable"
            );
        }
        assert!(
            GATEABLE_NAMESPACES.contains(&"subagent"),
            "the reserved subagent namespace must be gateable"
        );
        assert!(
            GATEABLE_NAMESPACES.contains(&"media"),
            "the real-money media namespace must be gateable"
        );
        assert!(
            GATEABLE_NAMESPACES.contains(&"composio"),
            "the per-tenant composio namespace must be gateable"
        );
        assert!(
            GATEABLE_NAMESPACES.contains(&"search"),
            "the metered search namespace must be gateable (issue #238)"
        );
    }

    /// Every namespace `namespace_of` can emit that is neither the Composio
    /// connection path nor the raw-HTTP `web` family must be in the shared
    /// native vocabulary — otherwise a future native tool would be wired but
    /// invisible to native-first routing (the brief and the classifier both key
    /// off that vocabulary).
    #[test]
    fn native_vocabulary_covers_every_native_mapped_namespace() {
        let mapped = [
            "shell",
            "read_workspace_state",
            "apply_patch",
            "git_operations",
            "csv_export",
            "web_fetch",
            "http_request",
            "curl",
            "image_info",
            "media_generate_image",
            "media_generate_video",
            "media_list_models",
            "composio_list_toolkits",
            "composio_list_connections",
            "composio_list_tools",
            "composio_authorize",
            "composio_execute",
            "web_search",
            "exa_find_similar",
            "exa_get_contents",
            "brave_news_search",
            "brave_image_search",
            "brave_video_search",
        ];
        let native: std::collections::HashSet<&str> =
            crate::company::native_capability_namespaces()
                .into_iter()
                .collect();
        for tool in mapped {
            let ns = namespace_of(tool).expect("mapped tool has a namespace");
            if ns == "composio" || ns == "web" {
                continue;
            }
            assert!(
                native.contains(ns),
                "native namespace `{ns}` (from `{tool}`) is not in the native vocabulary"
            );
        }
    }

    #[test]
    fn exec_security_shape_is_workspace_scoped_and_hardened() {
        let ws = Path::new("/tmp/oc-toolbelt-policy");

        let supervised = exec_security(ws, PolicyMode::Supervised);
        assert!(supervised.workspace_only, "must be workspace-only");
        assert_eq!(supervised.workspace_dir, ws);
        assert_eq!(supervised.action_dir, ws);
        assert!(
            supervised.block_high_risk_commands,
            "high-risk must be blocked"
        );
        assert!(
            !supervised.allow_tool_install,
            "tool install must be denied"
        );
        assert!(
            !supervised.auto_approve_all,
            "blanket auto-approve must be off"
        );
        assert_eq!(supervised.autonomy, AutonomyLevel::Supervised);
        assert!(
            supervised.require_approval_for_medium_risk,
            "supervised must approve medium-risk"
        );

        let readonly = exec_security(ws, PolicyMode::Readonly);
        assert_eq!(readonly.autonomy, AutonomyLevel::ReadOnly);
        assert!(!readonly.require_approval_for_medium_risk);

        let full = exec_security(ws, PolicyMode::Full);
        assert_eq!(full.autonomy, AutonomyLevel::Full);
        assert!(!full.require_approval_for_medium_risk);
    }

    /// `auto` must not loosen shell execution (issue #560).
    ///
    /// This is the test for the decision argued on [`autonomy_for`], and it
    /// guards a hole with no other guard: a workflow `tool_call` node has **no**
    /// `ApprovalPolicy` above it, so this policy is the entire tier there.
    /// `auto` is more permissive than `supervised` at the approval gate, and the
    /// tempting mapping — matching that feel with `AutonomyLevel::Full` — would
    /// silently drop the medium-risk shell gate for every workflow node on an
    /// `auto` company, because upstream's approval arm fires only when
    /// `autonomy == Supervised`.
    ///
    /// The second assertion is the subtler half. With autonomy mapped to
    /// `Supervised`, `require_approval_for_medium_risk` becomes load-bearing —
    /// and it was written as `mode == PolicyMode::Supervised`, an expression
    /// that was exhaustive by accident and answers `false` for a variant added
    /// beside it. Getting the mapping right and leaving that expression alone
    /// would have reopened the same hole from the other side.
    #[test]
    fn auto_borrows_supervised_exec_security_rather_than_full() {
        let ws = Path::new("/tmp/oc-toolbelt-policy-auto");
        let auto = exec_security(ws, PolicyMode::Auto);

        assert_eq!(
            auto.autonomy,
            AutonomyLevel::Supervised,
            "auto must not inherit Full's exec autonomy — a workflow tool_call node has no \
             approval gate above this policy"
        );
        assert!(
            auto.require_approval_for_medium_risk,
            "the medium-risk gate is inert unless autonomy is Supervised, so auto must opt in \
             explicitly or the mapping above buys nothing"
        );

        // The rest of the hardening is tier-independent and stays so.
        assert!(auto.block_high_risk_commands);
        assert!(!auto.allow_tool_install);
        assert!(!auto.auto_approve_all);
        assert!(auto.workspace_only);
    }

    /// The mapping above, proven where it actually bites: at openhuman's own
    /// command gate, on the class that separates the two candidate mappings.
    ///
    /// `auto_borrows_supervised_exec_security_rather_than_full` pins the two
    /// fields; this pins what they *do*.
    ///
    /// # Which class actually distinguishes the mappings
    ///
    /// Worth stating, because the intuitive example is the wrong one. At
    /// `gate_decision`, `Destructive` prompts under `Supervised` **and** under
    /// `Full` — so `rm -rf /` cannot tell the two mappings apart, and a test
    /// written around it would pass whichever mapping `autonomy_for` chose.
    /// (`block_high_risk_commands` is a separate, unconditional refusal on the
    /// `validate_command` path; it is not what `gate_decision` reports.)
    ///
    /// The one class `Full` actually loosens is `Write`: `Supervised` prompts,
    /// `Full` allows. That makes `Write` the whole of the difference here, and
    /// it is the ordinary case rather than an exotic one — an unrecognised
    /// command is classified `Write` by fail-closed default. So mapping `auto`
    /// to `Full` would have let routine state-changing shell commands run
    /// unprompted in workflow `tool_call` nodes, which is exactly the tier
    /// inversion `autonomy_for` argues against.
    ///
    /// Asserted on `CommandClass` directly rather than through
    /// `classify_command`, so this pins the tier decision and not the
    /// classifier's string heuristics.
    #[test]
    fn auto_gates_write_class_commands_exactly_as_supervised_does() {
        use oh::security::{CommandClass, GateDecision};
        let ws = Path::new("/tmp/oc-toolbelt-policy-auto-cmd");

        let auto = exec_security(ws, PolicyMode::Auto);
        let supervised = exec_security(ws, PolicyMode::Supervised);
        let full = exec_security(ws, PolicyMode::Full);

        // The load-bearing assertion: the class the two mappings disagree about.
        assert_eq!(
            auto.gate_decision(CommandClass::Write),
            GateDecision::Prompt,
            "a write-class command must still ask on an auto desk — mapping auto to Full would \
             let it run unprompted in a workflow tool_call node, which has no approval gate above \
             this policy"
        );
        assert_eq!(
            full.gate_decision(CommandClass::Write),
            GateDecision::Allow,
            "guard for the assertion above: if Full ever stops allowing Write, this test no \
             longer distinguishes the two mappings and must be rewritten"
        );

        // Everything else `auto` decides, it decides identically to `supervised`.
        for class in [
            CommandClass::Read,
            CommandClass::Write,
            CommandClass::Network,
            CommandClass::Install,
            CommandClass::Destructive,
        ] {
            assert_eq!(
                auto.gate_decision(class),
                supervised.gate_decision(class),
                "auto must gate {class:?} exactly as supervised does"
            );
        }

        // And it is not readonly either — an auto desk can still act.
        let readonly = exec_security(ws, PolicyMode::Readonly);
        assert_eq!(
            readonly.gate_decision(CommandClass::Write),
            GateDecision::Block
        );
    }

    #[tokio::test]
    async fn shell_rm_rf_denied_under_readonly_parked_under_supervised() {
        use oh::security::GateDecision;
        let ws = std::env::temp_dir();

        // Readonly: a destructive command is hard-blocked by the policy — proven
        // both at the decision layer and end-to-end (the tool refuses before it
        // spawns; the harmless nonexistent path is a belt-and-braces target).
        let readonly = test_security(&ws, PolicyMode::Readonly);
        assert_eq!(
            readonly.gate_decision(readonly.classify_command("rm -rf /")),
            GateDecision::Block,
            "readonly must hard-block destructive commands"
        );
        let tool = ShellTool::new(readonly, native_runtime(), AuditLogger::disabled());
        let result = tool
            .execute(json!({ "command": "rm -rf /tmp/oc-toolbelt-nonexistent-xyz" }))
            .await
            .unwrap();
        assert!(
            result.is_error,
            "readonly rm -rf must error: {}",
            result.output()
        );
        assert!(result.output().to_lowercase().contains("read-only"));

        // Supervised: the destructive command is *parked* (requires approval),
        // never auto-allowed. The park→resolve step is opencompany's own
        // `ApprovalPolicy` gate layered above; here we prove OpenHuman's policy
        // classifies it as approval-required rather than allowed.
        let supervised = test_security(&ws, PolicyMode::Supervised);
        assert_eq!(
            supervised.gate_decision(supervised.classify_command("rm -rf /")),
            GateDecision::Prompt,
            "supervised must park (require approval for) destructive commands"
        );
    }

    #[tokio::test]
    async fn web_tools_reject_ssrf_ip_literals() {
        let ws = std::env::temp_dir();
        let security = test_security(&ws, PolicyMode::Full);
        // Open-public allowlist: proves the IP guard fires independently of the
        // domain allowlist. Both are IP literals, so rejection short-circuits
        // before any DNS lookup or network I/O.
        let tools = web_tools(security, Vec::new(), &ws);
        for tool in &tools {
            // Only web_fetch / http_request take a plain `url` arg.
            if !matches!(tool.name(), "web_fetch" | "http_request") {
                continue;
            }
            for url in ["http://169.254.169.254/", "http://127.0.0.1:1/"] {
                let result = tool.execute(json!({ "url": url })).await.unwrap();
                assert!(
                    result.is_error,
                    "{} must reject SSRF target {url}: {}",
                    tool.name(),
                    result.output()
                );
            }
        }
    }

    #[tokio::test]
    async fn apply_patch_denied_outside_workspace() {
        // A private workspace root, not a fixed `/tmp` name: the old fixed name
        // made two concurrent runs of this test tear down each other's
        // directory between the create and the assertion.
        let ws_dir = tempfile::Builder::new()
            .prefix("oc-toolbelt-escape-")
            .tempdir()
            .expect("tempdir");
        let ws = ws_dir.path();
        let security = test_security(ws, PolicyMode::Full);
        let tool = ApplyPatchTool::new(security);
        // A path escaping the workspace root must be refused by the policy.
        let result = tool
            .execute(json!({
                "edits": [
                    { "path": "../outside.txt", "old_string": "x", "new_string": "y" }
                ]
            }))
            .await
            .unwrap();
        assert!(
            result.is_error,
            "workspace escape must be denied: {}",
            result.output()
        );
    }

    #[test]
    fn filter_allow_all_is_identity() {
        let ws = Path::new("/tmp/oc-toolbelt-filter");
        let security = test_security(ws, PolicyMode::Supervised);
        let mut tools = shell_tools(
            security.clone(),
            native_runtime(),
            Some(ShellAudit::disabled()),
            ws,
        );
        tools.extend(code_tools(security, ws));
        // Own the names before `tools` moves into the filter.
        let before: Vec<String> = tools.iter().map(|t| t.name().to_string()).collect();
        let after_tools = filter_by_capabilities(tools, &CapabilityFilter::AllowAll);
        let after: Vec<String> = after_tools.iter().map(|t| t.name().to_string()).collect();
        assert_eq!(before, after, "AllowAll must be identity");
    }

    /// The `media` toolbelt (issue #109) exposes exactly the three OpenHuman
    /// media tools, each mapped to the `media` namespace so the capability filter
    /// gates them as one real-money family. Only built under the `media` feature.
    #[cfg(feature = "media")]
    #[test]
    fn media_tools_expose_expected_names_and_namespace() {
        let ws = Path::new("/tmp/oc-toolbelt-media");
        let backend = MediaBackend {
            backend_url: "https://api.tinyhumans.ai".to_string(),
            auth_token: "managed-token".to_string(),
        };
        let tools = media_tools(&backend, ws);
        let got = names(&tools);
        for expected in [
            "media_generate_image",
            "media_generate_video",
            "media_list_models",
        ] {
            assert!(got.contains(&expected), "missing {expected}: {got:?}");
        }
        assert_eq!(got.len(), 3, "media tools drifted: {got:?}");
        for tool in &tools {
            assert_eq!(
                namespace_of(tool.name()),
                Some("media"),
                "media_tools leaked a non-media tool: {}",
                tool.name()
            );
        }
    }

    /// The managed credential never lands in a `MediaBackend` debug trace.
    #[test]
    fn media_backend_debug_redacts_the_token() {
        let backend = MediaBackend {
            backend_url: "https://api.tinyhumans.ai".to_string(),
            auth_token: "super-secret".to_string(),
        };
        let shown = format!("{backend:?}");
        assert!(!shown.contains("super-secret"), "token leaked: {shown}");
        assert!(shown.contains("<redacted>"), "{shown}");
        assert!(shown.contains("api.tinyhumans.ai"), "{shown}");
    }

    /// A backend URL that is not exactly `https` must never reach
    /// `IntegrationClient::new`: the client attaches the managed token, so an
    /// `http://` override would send it in the clear. Fail closed with no tools.
    #[cfg(feature = "media")]
    #[test]
    fn a_non_https_media_backend_wires_no_tools() {
        let ws = Path::new("/tmp/oc-toolbelt-media-http");
        for url in [
            "http://api.tinyhumans.ai",
            "ftp://api.tinyhumans.ai",
            "not-a-url",
        ] {
            let backend = MediaBackend {
                backend_url: url.to_string(),
                auth_token: "managed-token".to_string(),
            };
            let tools = media_tools(&backend, ws);
            assert!(
                tools.is_empty(),
                "backend `{url}` must not construct any media tool"
            );
        }
    }

    #[test]
    fn filter_deny_drops_mapped_but_keeps_intrinsic() {
        // Mix a real intrinsic tool (`file_read`, unmapped) with mapped exec
        // tools; a full namespace deny must keep only the intrinsic one.
        let ws = Path::new("/tmp/oc-toolbelt-deny");
        let security = test_security(ws, PolicyMode::Supervised);
        let mut tools: Vec<Box<dyn Tool>> = shell_tools(
            security.clone(),
            native_runtime(),
            Some(ShellAudit::disabled()),
            ws,
        );
        tools.extend(code_tools(security.clone(), ws));
        tools.extend(web_tools(security.clone(), Vec::new(), ws));
        // `file_read` has no mapped namespace → intrinsic → always kept.
        tools.push(Box::new(oh::tools::FileReadTool::new(security)));

        let deny: HashSet<&'static str> = ["shell", "code", "web"].into_iter().collect();
        let kept = filter_by_capabilities(tools, &CapabilityFilter::DenyNamespaces(deny));
        let kept_names = names(&kept);
        assert_eq!(
            kept_names,
            vec!["file_read"],
            "only the intrinsic tool must survive a full deny: {kept_names:?}"
        );
    }

    /// [`namespace_denied`] must agree with [`filter_by_capabilities`] on every
    /// case: it is the standalone check `build_agent` uses to keep the sandbox
    /// brief from describing a namespace the filter is about to strip from the
    /// tool vector, so a mismatch between the two would let the brief and the
    /// live belt disagree again — the exact bug this function exists to close.
    #[test]
    fn namespace_denied_agrees_with_filter_by_capabilities() {
        assert!(!namespace_denied(&CapabilityFilter::AllowAll, "shell"));
        assert!(!namespace_denied(&CapabilityFilter::AllowAll, "code"));

        let deny: HashSet<&'static str> = ["shell"].into_iter().collect();
        let filter = CapabilityFilter::DenyNamespaces(deny);
        assert!(namespace_denied(&filter, "shell"));
        assert!(!namespace_denied(&filter, "code"));

        // A namespace outside `DenyNamespaces`' set is simply not denied — it
        // is never asked to special-case a name it does not recognize.
        assert!(!namespace_denied(&filter, "web"));
    }

    /// The grant/credential resolving `wired = true` is not enough: a
    /// capability tier that has denied `composio` (budget exhausted, or a
    /// fail-closed metering error) must still turn the predicate off, because
    /// `filter_by_capabilities` is about to strip every `composio_*` tool from
    /// the belt. This is the fix for the P1 codex found on PR #1780 — before
    /// it, the S1 brief and S2 deflection policy were wired from the grant
    /// alone, exactly the shape `sandbox_brief_flags_withhold_a_capability_denied_namespace`
    /// (PR #1670) fixed for `shell`/`code`.
    #[test]
    fn composio_capability_admits_withholds_when_the_tier_denies_it() {
        assert!(
            composio_capability_admits(true, &CapabilityFilter::AllowAll),
            "wired + no denial must admit"
        );
        assert!(
            !composio_capability_admits(false, &CapabilityFilter::AllowAll),
            "not wired must never admit, regardless of the tier"
        );

        let deny_composio = CapabilityFilter::DenyNamespaces(["composio"].into_iter().collect());
        assert!(
            !composio_capability_admits(true, &deny_composio),
            "wired but denied must not admit — the brief/policy must not \
             describe a surface `filter_by_capabilities` is about to strip"
        );

        // A denial of an unrelated namespace must not withhold composio.
        let deny_shell = CapabilityFilter::DenyNamespaces(["shell"].into_iter().collect());
        assert!(
            composio_capability_admits(true, &deny_shell),
            "a denial of another namespace must not withhold composio"
        );
    }
}
