//! What a tool can **reach** — the one declaration both approval questions read.
//!
//! ## Two questions, one declaration (issues #441, #443, #444)
//!
//! The approval gate asks two different things about every tool call:
//!
//! 1. **May this run unattended?** — answered by [`Reach`]. `readonly` denies
//!    anything that mutates or reaches outside; `supervised` parks it.
//! 2. **May an operator hand this over for a stretch of time?** — answered by
//!    [`Standing`], the standing-grant boundary.
//!
//! Until now both were read off one value: the [`EffectGroup`] the tool name
//! was pattern-matched into. That made the residual `Other` bucket mean two
//! unrelated things at once — "no particular consequence to name on the card"
//! *and* "safe to grant for a week" — so the three broadest capabilities in the
//! system (`shell`, `http_request`, `workspace_write`) were grantable because
//! their names contain no consequence word, while every Composio action was
//! ungrantable because they all arrive under one tool name that reads as a send.
//!
//! They are separate questions and they now have separate answers, derived from
//! **one** declaration per tool so they cannot drift apart again.
//! `is_external_effect` and `classify_group` in
//! [`crate::harness::policy`] are both thin readers of [`consequence_of`], and
//! [`Effect::may_be_granted_standing`](crate::ports::types::Effect::may_be_granted_standing)
//! — the mint-side rule, in the default build where the harness does not compile
//! — is a third.
//!
//! ## Why the table names tools rather than matching their names
//!
//! A name's vocabulary is not a property of what a tool can do. `shell` carries
//! no consequence word and runs arbitrary code; `file_read` carries no
//! *read-only* prefix and reads a file. Every previous fix here added one more
//! carve-out to a hand-maintained list, and the failure mode when somebody
//! forgot was silent — the tool simply started asking for permission, and the
//! person who noticed was an operator wondering why a read needed approving.
//!
//! So the declaration is explicit and the coverage is enforced:
//! `every_registered_tool_is_declared` in [`crate::harness`] builds every belt
//! the crate can wire and fails if a live tool is missing from [`DECLARED`].
//! Adding a tool without classifying it breaks a test rather than an operator's
//! afternoon.
//!
//! ## Unknown means cautious, in both directions
//!
//! An undeclared tool keeps the old name heuristics for [`Reach`] — dropping
//! them would park a `read_*` tool from a build configuration nobody tested —
//! but it is **never** [`Standing::Grantable`]. A tool nobody has thought about
//! must not inherit a week-long capability by omission.
//!
//! The Composio arm says the same thing with one more step (issue #1818). A slug
//! the curated catalogue cannot place is read for the *verb it names*: an action
//! that positively says it lists, gets, fetches or searches is an
//! [`Reach::ExternalRead`], and anything else — a mutating verb, or no verb this
//! module recognises at all — is still a **send**. Cautious is the default, but
//! the default is not applied to a slug that has already told you it only reads.

use crate::ports::types::EffectGroup;

/// What a call **costs the company** — the axis the two policy tiers cut on.
///
/// Named for consequence rather than for topology, because topology is the
/// wrong question and asking it is what produced the bug this module exists to
/// fix. Several tools make a real network request and are nonetheless
/// [`Nothing`](Self::Nothing): `composio_list_tools`, `media_list_models` and
/// `mcp_list_tools` all fetch a catalogue over the wire with the tenant's own
/// credential, change no state anywhere, and are billed for nothing. A tier
/// that denied them would be denying a company the ability to find out what it
/// can do.
///
/// * `readonly` denies anything that is not [`Nothing`](Self::Nothing) — that
///   tier's contract is that nothing changes and nothing is spent.
/// * `supervised` parks only [`Consequence`](Self::Consequence).
///   [`Money`](Self::Money) is the third bucket `web_search` needed (issue
///   #238): it changes nothing but the backend bills per request, and parking
///   it would be worse than useless — openhuman resolves a `RequireApproval`
///   inline, so a parked search is a search that never happens.
///   [`ExternalRead`](Self::ExternalRead) is the fourth (issue #559), for the
///   same reason with the billing removed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reach {
    /// Nothing changes and nothing is spent. Runs in every mode, `readonly`
    /// included. May still read over the network — see the note above.
    Nothing,
    /// Nothing changes, but the call is billed. Runs under `supervised`,
    /// denied under `readonly`.
    Money,
    /// A third party's own data is read with the company's connected
    /// credential. Nothing changes anywhere and nothing is billed, but the
    /// account being read is not the company's own — so `supervised` allows it
    /// and `readonly` still denies it (issue #559).
    ///
    /// Distinct from [`Money`](Self::Money) rather than folded into it, and the
    /// reason is [`costs_money`](Self::costs_money): that predicate feeds the
    /// daily spend cap, so reusing `Money` here would bill an operator for
    /// every page of every mailbox they read. Distinct from
    /// [`Nothing`](Self::Nothing) because a `readonly` desk reaching into a
    /// counterparty's account is exactly what that tier promises not to do.
    ExternalRead,
    /// State changes, a counterparty is reached, arbitrary code runs, an
    /// arbitrary address is reached, or operator-owned guidance is overwritten.
    /// Parks under `supervised`, denied under `readonly`.
    Consequence,
}

impl Reach {
    /// Is this refused outright on a `readonly` desk?
    pub fn denied_under_readonly(self) -> bool {
        !matches!(self, Self::Nothing)
    }

    /// Does this park for an operator under `supervised`?
    pub fn parks_under_supervision(self) -> bool {
        matches!(self, Self::Consequence)
    }

    /// Does making this call cost money, whatever it changes?
    pub fn costs_money(self) -> bool {
        matches!(self, Self::Money)
    }
}

/// May an operator open this tool up for a stretch of time, or is every call
/// its own decision (issue #444)?
///
/// Decided by what the tool can **reach**, never by what it is called. A tool
/// that can execute arbitrary code, reach an arbitrary address, or overwrite
/// operator-owned state is [`PerCall`](Self::PerCall) however innocuous its
/// name; a read scoped to one connected account is
/// [`Grantable`](Self::Grantable) however alarming the tool carrying it sounds.
///
/// # This now decides two different things (issue #560)
///
/// Since the `auto` tier, [`Consequence::parks_under_auto`] reads this field to
/// mean "may run **unattended for everyone** while the company sits in `auto`"
/// — a wider grant than the per-teammate, until-a-deadline one the name
/// describes. Loosening a tool to [`Grantable`](Self::Grantable) for a
/// delegation reason therefore also stops it parking under `auto`, for every
/// agent, with no operator in the loop. That is sound because this field is
/// decided by what a tool can *reach* rather than by what it is called — but it
/// is two decisions in one edit, and the second one is easy to make by
/// accident. `the_auto_tier_line_is_pinned_tool_by_tool` in this module's tests
/// walks the whole table and fails loudly if a tool crosses that line.
///
/// # Which is why the two questions are now separable (issue #673)
///
/// [`ScopedGrantable`](Self::ScopedGrantable) answers the first question `yes`
/// and the second `no`: an operator may delegate it to one teammate until a
/// deadline, while the call's [`Reach`] still decides whether it parks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Standing {
    /// An operator may grant this to a teammate until a deadline — **and**,
    /// since issue #560, may run unattended under the `auto` tier. See the note
    /// on [`Standing`] before loosening a tool to this.
    Grantable,
    /// An operator may grant this to a teammate until a deadline, but the
    /// standing itself does not confer unattended execution under `auto`.
    ///
    /// This preserves scoped delegation without making standing itself confer
    /// unattended execution.
    ///
    /// A tool declared this way is only ever grantable **with a scope**: its
    /// declaration is argument-classified, so a call whose scope cannot be
    /// derived falls back to [`PerCall`](Self::PerCall) rather than minting an
    /// unscoped grant. That matters because
    /// [`StandingGrant::admits_scope`](crate::runtime::grants::StandingGrant::admits_scope)
    /// treats an unscoped grant as admitting *everything* — correct for a
    /// journal line predating the field, catastrophic for a fetch grant that
    /// failed to name a host.
    ScopedGrantable,
    /// Every call is its own decision.
    PerCall,
}

impl Standing {
    /// May this be granted standing to one teammate until a deadline?
    ///
    /// True for both grantable variants. This is the mint-and-spend question —
    /// the one the field is named for — and it is deliberately **not** the
    /// question the `auto` tier asks; see
    /// [`runs_unattended_under_auto`](Self::runs_unattended_under_auto).
    pub fn is_grantable(self) -> bool {
        matches!(self, Self::Grantable | Self::ScopedGrantable)
    }

    /// May this run unattended, for every agent, while the company sits in
    /// `auto` (issue #560)?
    ///
    /// Split out from [`is_grantable`](Self::is_grantable) by issue #673. The
    /// two used to be the same predicate, which meant reaching for a standing
    /// grant on any tool also stopped it parking under `auto`. They are
    /// different sentences an operator consents to, and only
    /// [`Grantable`](Self::Grantable) means both.
    pub fn runs_unattended_under_auto(self) -> bool {
        matches!(self, Self::Grantable)
    }
}

/// Everything the approval gate needs to know about one tool call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Consequence {
    /// The consequence class the operator's approval card names.
    pub group: EffectGroup,
    /// What the call mutates or reaches.
    pub reach: Reach,
    /// Whether it can be granted standing.
    pub standing: Standing,
}

impl Consequence {
    /// Does this park for an operator under `auto` (issue #560)?
    ///
    /// `auto` is the tier between `supervised` — which parks every write and
    /// every outward read, so companies drown — and `full`, which parks nothing
    /// but the `always_approve` list. Its contract, in the operator's words:
    /// **the agent works without interrupting me, and stops before anything
    /// that leaves the building or spends money.**
    ///
    /// # Why this reads two fields instead of adding a third
    ///
    /// The split `auto` needs is already declared. [`Standing::Grantable`]
    /// marks exactly the calls whose consequence stays inside this company —
    /// the agent's own scratch writes (`file_write`, `edit`, `apply_patch`,
    /// `csv_export`, `memory_store`) and a read scoped to one connected account.
    /// Everything that can execute arbitrary code, reach an arbitrary address,
    /// overwrite operator-authored guidance, spend on generation, or perform an
    /// effect this layer cannot see is [`Standing::PerCall`] — deliberately, and
    /// argued tool by tool in [`DECLARED`]. So the tier is a *reader* of that
    /// work, not a second table to be kept in step with it. A fresh list would
    /// be the exact hand-maintained carve-out this module was written to delete,
    /// and it would drift the same silent way.
    ///
    /// # What the operator is consenting to
    ///
    /// [`Standing`] answers "may an operator hand this to one teammate until a
    /// deadline?", and this reuses it to mean "may it run unattended for
    /// everyone while the company sits in `auto`?" — which is a genuinely wider
    /// grant than a standing grant, not the same one. It is sound because
    /// [`Standing`] is decided by what a tool can *reach* rather than by how
    /// alarming its name is, and because the widening is exactly the choice the
    /// operator makes when they select the tier. It is recorded here so a future
    /// edit that loosens `Grantable` knows it is loosening two things.
    ///
    /// # Two boundaries this does not draw
    ///
    /// [`Reach::Money`] does **not** park. `web_search` is billed but changes
    /// nothing, and it already runs unattended under `supervised` for a reason
    /// that binds harder here: openhuman resolves a `RequireApproval` inline and
    /// never re-dispatches, so a parked search is a search that never happens
    /// and an agent with no search invents citations. `auto` must not be
    /// stricter than the tier it replaces. The per-agent daily cap is the
    /// boundary that actually holds spend, and it sits above the tier dispatch.
    /// Generation that spends on *submit* — `media_generate_image`,
    /// `media_generate_video` — is [`Reach::Consequence`] and `PerCall`, so it
    /// parks.
    ///
    /// `always_approve` is not consulted here either: it is checked above the
    /// tier dispatch in
    /// [`ApprovalPolicy::check`](crate::harness::policy::ApprovalPolicy) and
    /// wins over every tier, `full` included.
    ///
    /// # Stable across issue #559
    ///
    /// A Composio read is `Grantable` today while still carrying
    /// [`Reach::Consequence`]; #559 reclassifies its *reach* without touching
    /// its standing. This predicate returns `false` — runs unattended — in both
    /// worlds, because the `Grantable` half already decides it. The two changes
    /// can land in either order and neither can silently invert the other.
    /// # Reads the narrower of the two questions since issue #673
    ///
    /// This used to read `is_grantable()`, which fused "may be delegated to one
    /// teammate" with "runs unattended for everyone under `auto`". An outward
    /// fetch needs the first and must not have the second, so the predicate it
    /// reads is now [`Standing::runs_unattended_under_auto`] and
    /// [`Standing::ScopedGrantable`] sits on the parking side of this line.
    pub fn parks_under_auto(self) -> bool {
        self.reach.parks_under_supervision() && !self.standing.runs_unattended_under_auto()
    }
}

/// One tool's declaration.
struct Declared {
    tool: &'static str,
    group: EffectGroup,
    reach: Reach,
    standing: Standing,
}

/// The Composio action-running tool. Its consequence is a property of the
/// *action* in its arguments, not of this name — see
/// [`composio_execute_consequence`].
pub const COMPOSIO_EXECUTE: &str = "composio_execute";

/// The argument key `composio_execute` carries the action slug under, on the
/// wire and in both this crate's tool and openhuman's.
///
/// `pub(crate)` so the test fixtures in
/// [`crate::policy::test_support`] build their arguments from the same constant
/// this classifier reads. Issue #470: fixtures across five modules hard-coded a
/// key of their own, the two drifted apart, and every one of those tests
/// silently stopped reaching the catalogue lookup it claimed to cover.
pub(crate) const COMPOSIO_ACTION_KEY: &str = "tool";

/// The shell tool, classified by the command it was handed rather than by this
/// name (issue #875).
pub const SHELL: &str = "shell";

/// The git tool, classified by the `operation` it was handed rather than by
/// this name (issue #877).
pub const GIT_OPERATIONS: &str = "git_operations";

/// The argument `git_operations` names its subcommand in. Shared with the
/// fixtures for the reason [`COMPOSIO_ACTION_KEY`] is: a key hard-coded
/// separately in a test is a test that stops reaching the classifier without
/// saying so (issue #470).
pub(crate) const GIT_OPERATION_KEY: &str = "operation";

/// The argument key [`SHELL`] carries the command line under.
///
/// A required parameter of the vendored tool's schema, so a call that omits it
/// could not have run anyway; a call this cannot read stays gated.
pub(crate) const SHELL_COMMAND_KEY: &str = "command";

/// The optional argument the model may use to declare what its own command
/// does. Read **escalate-only**, exactly as upstream reads it: a self-declared
/// class may raise the requirement, never lower it. A model that could talk its
/// way down a tier by labelling `rm -rf` a read would be the whole gate.
pub(crate) const SHELL_CATEGORY_KEY: &str = "category";

/// An outward-fetch tool whose standing grant may be scoped to a host.
pub const WEB_FETCH: &str = "web_fetch";

/// The argument key [`WEB_FETCH`] carries its absolute URL under.
///
/// A required parameter of the vendored tool's schema, so a call that omits it
/// could not have run anyway; a call this cannot read simply stays `PerCall`.
pub(crate) const WEB_FETCH_URL_KEY: &str = "url";

/// The bridge tool that calls through a company-declared MCP server (#1124).
///
/// Argument-classified because one name carries every remote tool on every
/// server: filing a Jira ticket and reading one arrive here identically, so
/// classifying the *name* charged the operator the same approval card for both.
/// The (server, tool) pair the call already carries is what separates them —
/// see [`mcp_call_tool_consequence`].
pub const MCP_CALL_TOOL: &str = "mcp_call_tool";

/// The bridge tool that calls through a **registry**-installed MCP server
/// (#1124) — a different store from [`MCP_CALL_TOOL`]'s, keyed by `server_id`
/// rather than by name, but graded on the same declaration for the same reason.
pub const MCP_REGISTRY_TOOL_CALL: &str = "mcp_registry_tool_call";

/// The argument key [`MCP_CALL_TOOL`] names its server under. A required
/// parameter of the tool's schema (`OcMcpCallTool::parameters_schema`), so a
/// call this cannot read could not have run — and stays gated.
pub(crate) const MCP_CALL_SERVER_KEY: &str = "server";

/// The argument key [`MCP_CALL_TOOL`] names the remote tool under.
pub(crate) const MCP_CALL_TOOL_KEY: &str = "tool";

/// The argument key [`MCP_REGISTRY_TOOL_CALL`] names its server under —
/// `server_id`, not `server`: the registry addresses installs by a stable id,
/// not by the display name the [`MCP_CALL_TOOL`] path uses.
pub(crate) const MCP_REGISTRY_SERVER_KEY: &str = "server_id";

/// The argument key [`MCP_REGISTRY_TOOL_CALL`] names the remote tool under —
/// `tool_name`, not `tool`, matching the vendored `McpRegistryToolCallTool`
/// schema.
pub(crate) const MCP_REGISTRY_TOOL_KEY: &str = "tool_name";

/// Every tool this crate can wire onto an agent, and what it can reach.
///
/// Ordered by family for reading, not by any semantic. The **coverage test**
/// (`every_registered_tool_is_declared`) is what keeps it complete; the
/// **constant test** below is what keeps the literals here tied to the
/// `*_TOOL` constants the tools themselves return.
const DECLARED: &[Declared] = &[
    // ---- Orchestration: in-cycle work this company hands to itself ---------
    // These enqueue a task card or a hand-off the harness brain drains in the
    // same turn. Nothing leaves the company (issue #53). None is grantable:
    // an internal tool never parks, so its standing answer is unobservable
    // *unless* an operator puts it in `always_approve` — at which point
    // `PerCall` is the answer that respects what they asked for.
    d("query_company", EffectGroup::Other, Reach::Nothing),
    d("spawn_task", EffectGroup::Other, Reach::Nothing),
    d("delegate_to_desk", EffectGroup::Other, Reach::Nothing),
    // Issue #884: `delegate_to_teammate` is `delegate_to_desk` resolved to a
    // person instead of a desk. Same class exactly — it runs a turn inside this
    // company and nothing leaves it.
    d("delegate_to_teammate", EffectGroup::Other, Reach::Nothing),
    d("add_agent", EffectGroup::Other, Reach::Nothing),
    d("create_workflow", EffectGroup::Other, Reach::Nothing),
    d("assign_task", EffectGroup::Other, Reach::Nothing),
    d("review_task", EffectGroup::Other, Reach::Nothing),
    // Issue #1861: `escalate_to_human` stages a question on this company's own
    // approval queue and nothing leaves the company — the same class as
    // `spawn_task`, which also puts something in front of the operator. It is
    // strictly *less* consequential than the card: a card assigns work and can
    // be dispatched, whereas an unanswered question expires through the
    // approval TTL having changed nothing.
    //
    // `Reach::Nothing` also has to hold for the tool to be usable at all. The
    // gate guessing from the name would be free to park the escalation itself,
    // which would ask the operator to approve being asked a question.
    d("escalate_to_human", EffectGroup::Other, Reach::Nothing),
    // Issue #661 (M7). `read_workflow` is a pure read of this company's own
    // saved graphs — the same class as `query_company`, which already lists
    // them.
    d("read_workflow", EffectGroup::Other, Reach::Nothing),
    // `update_workflow` takes `create_workflow`'s classification, and gets it by
    // re-deriving rather than by copying. Four properties separate it from
    // `workspace_write`, which is the tool it superficially resembles and which
    // is deliberately `Reach::Consequence`:
    //
    //  * every content-changing update is **undoable by construction** — issue
    //    #274 snapshots the prior body inside the same write lock, so the thing
    //    an operator would want to have seen is still there afterwards;
    //  * the tool refuses a scheduled target, so every workflow it CAN edit is
    //    manual-run only: a bad edit's consequence materialises solely through
    //    `run_workflow`, which parks;
    //  * `expected_version` is required, so it cannot clobber state nobody read;
    //  * validation is identical to the console's, including #682's per-kind
    //    config rules — an agent edit cannot persist a graph an operator's
    //    could not.
    //
    // Parking every fix-up save of a draft the agent itself just created would
    // also recreate the #558/#561 no-consequence-interrupt pattern against the
    // exact flow M7 exists to enable.
    //
    // The honest residual, recorded rather than quietly carried: `Reach::Nothing`
    // means a `readonly` desk can edit a workflow. But `create_workflow` above is
    // already `Nothing`, so such a desk can already author one — this is a
    // pre-existing classification, not a new hole. If it is wrong, create and
    // update are reclassified TOGETHER; neither moves alone.
    d("update_workflow", EffectGroup::Other, Reach::Nothing),
    // `delete_workflow` does not, and the argument is `workspace_delete`'s
    // (#671) almost verbatim — with the object strictly worse. Deleting a
    // workflow removes the graph AND cascades its whole #274 revision history
    // away in the same call, so unlike an update there is no prior body left to
    // restore from and unlike a workspace delete there is no artifact chain
    // outliving it. `PerCall` for #671's second reason too: a standing grant on
    // deletion is the shape that turns one bad turn into a company whose
    // processes are quietly gone by the end of it, and per-call parking makes
    // each removal its own card naming its own workflow.
    d("delete_workflow", EffectGroup::Other, Reach::Consequence),
    // Running a saved workflow performs whatever that workflow performs, which
    // this layer cannot see. It parks, and it stays a per-call decision.
    d("run_workflow", EffectGroup::Other, Reach::Consequence),
    // Reading a cached run's node output back (issue #418) reaches nothing: it
    // is a pure read of this process's own in-memory cache, no counterparty and
    // nothing an operator authored — same class as `query_company`.
    d("read_run_output", EffectGroup::Other, Reach::Nothing),
    // Issue #1859: `list_tasks` / `read_task` / `read_run` are the read-only
    // surface over the company's own task board — `TaskStore`, `RunStore` and
    // `ArtifactStore`/`EventLog`, none of which carries a run's USD cost, a raw
    // tool-call argument, or a step's full trace (see the fail-closed note on
    // `ListTasksTool`). No counterparty is reached and nothing changes, the
    // same class as `query_company` and `read_run_output` above.
    d("list_tasks", EffectGroup::Other, Reach::Nothing),
    d("read_task", EffectGroup::Other, Reach::Nothing),
    d("read_run", EffectGroup::Other, Reach::Nothing),
    // ---- The agent's own sandboxed workspace: reads ------------------------
    // All six are pure reads inside the workspace the agent is pinned to.
    // `file_read`, `glob`, `grep` and `image_info` PARKED before this table
    // existed — not by anyone's decision, but because the read-only-prefix
    // heuristic keys on the *start* of the name and none of them begins with
    // one. `list` and `memory_recall` happened to.
    //
    // `read_workspace_state` was the seventh member of this list until issue
    // #459; it is classified with `shell` below, for the reason given there.
    d("file_read", EffectGroup::Other, Reach::Nothing),
    d("glob", EffectGroup::Other, Reach::Nothing),
    d("grep", EffectGroup::Other, Reach::Nothing),
    d("list", EffectGroup::Other, Reach::Nothing),
    d("memory_recall", EffectGroup::Other, Reach::Nothing),
    // Queues an internal operator question. It does not perform the proposed
    // action and must stay callable even while the company is read-only.
    d("request_approval", EffectGroup::Other, Reach::Nothing),
    d("image_info", EffectGroup::Other, Reach::Nothing),
    // ---- The agent's own sandboxed workspace: writes -----------------------
    // These mutate, so `readonly` must still deny them and `supervised` must
    // still park them. But what they mutate is the agent's own scratch space
    // and this company's own memory — no counterparty, no arbitrary address,
    // nothing an operator authored. They are the low-consequence tools the
    // standing grant exists for: without them the feature has almost nothing
    // left to apply to.
    d_grantable("file_write", EffectGroup::Other, Reach::Consequence),
    d_grantable("edit", EffectGroup::Other, Reach::Consequence),
    d_grantable("apply_patch", EffectGroup::Other, Reach::Consequence),
    d_grantable("csv_export", EffectGroup::Other, Reach::Consequence),
    d_grantable("memory_store", EffectGroup::Other, Reach::Consequence),
    // Per-call, like every other delete in this table (`delete_workflow`,
    // `workspace_delete`, `pages_delete`) and for their stated reason: a
    // standing grant on deletion is the shape that turns one bad turn into a
    // memory that is quietly empty by the end of it. The own-prefix
    // confinement is not grounds for a lower price — a memory row has no
    // revision history and no artifact chain, so a wrong forget is simply
    // gone.
    d("memory_forget", EffectGroup::Other, Reach::Consequence),
    // `git_operations` is deliberately NOT grantable alongside its filesystem
    // siblings: it can push to a configured remote, so it reaches an address
    // this layer does not get to see.
    d("git_operations", EffectGroup::Other, Reach::Consequence),
    // ---- Arbitrary code, arbitrary addresses -------------------------------
    // The broad execution and network shapes. A standing grant on any of these
    // is a standing grant on "anything the sandbox permits", which is not a
    // sentence an operator can consent to.
    d("shell", EffectGroup::Other, Reach::Consequence),
    // `read_workspace_state` sits here rather than with its fellow workspace
    // reads because of what it does, not what it is called (issue #459). It
    // shells out to `git status` and `git log` in
    // `{root}/{company}/{agent}/workspace` — the same directory `file_write`
    // writes into — and the vendored `run_git` sets no `GIT_CONFIG_NOSYSTEM`,
    // no `-c` overrides and no environment scrub. Several git config keys name
    // a command to run and `git status` invokes `core.fsmonitor`, so a
    // `.git/config` the agent authored decides what executes. That is the
    // `shell` shape wearing a read's name, and `namespace_of` already maps it
    // into the `shell` namespace for capability gating.
    //
    // This is a consistency fix rather than a judgement call: `git_operations`
    // below is already `Reach::Consequence`, and its `run_git_command_in` is
    // the same unscrubbed `Command::new("git")`. The identical primitive was
    // gated in one tool and open in the other; `read_workspace_state` was the
    // odd one out.
    //
    // THIS IS A STOPGAP, and deliberately the blunt one: it costs an approval
    // on a routine orientation step. The fix that restores the ergonomics is
    // upstream, in openhuman's `run_git`
    // (`src/openhuman/tools/impl/system/workspace_state.rs`). It is not merely
    // unwritten — it is not straightforward: the exposure is the *repository*
    // config in an agent-writable directory, which `GIT_CONFIG_NOSYSTEM` and
    // `GIT_CONFIG_GLOBAL` do not reach, so a real fix has to refuse unknown
    // config rather than scrub a list of known-bad keys. That path is
    // byte-identical on openhuman `main` today, so there is no pin to bump to.
    // Revert this to `Reach::Nothing` once a hardened `run_git` is vendored,
    // and not before.
    //
    // The revert condition is tracked where the work has to happen —
    // tinyhumansai/openhuman#5494 — not only in this comment. A stopgap whose
    // removal condition lives as prose next to the stopgap, in a different repo
    // from its fix, is how these become permanent: nothing surfaces it when
    // somebody bumps the openhuman pin.
    d(
        "read_workspace_state",
        EffectGroup::Other,
        Reach::Consequence,
    ),
    d("http_request", EffectGroup::Other, Reach::Consequence),
    d("curl", EffectGroup::Other, Reach::Consequence),
    // `web_fetch` keeps its row so `declared_tools` still walks it, but its
    // standing is decided from the call's URL — see `web_fetch_consequence`
    // (issue #673). This row's `PerCall` is the answer for a call whose URL
    // cannot be read, which is exactly what that function falls back to.
    d(WEB_FETCH, EffectGroup::Other, Reach::Consequence),
    // ---- The company workspace: the shared note tree ------------------------
    // Reads are free (issue #237). `workspace_write` overwrites guidance the
    // operator wrote, which is why `is_external_effect` has always refused to
    // exempt it — and why it is now also refused a standing grant. That
    // contradiction (park every time / grant for a week) is issue #444's
    // headline, resolved in the direction the parking side already argued.
    //
    // `workspace_create` (issue #551) takes the identical classification, and
    // gets it for the same reason rather than by copying: it adds a node to the
    // tree every other agent and the operator read, unconfined by any prefix,
    // so it reaches past this turn exactly as an overwrite does. Anything
    // weaker would also be incoherent — a standing grant to *add* notes beside
    // a per-call gate on *editing* them buys an agent the ability to fill the
    // tree without ever asking.
    d("workspace_list", EffectGroup::Other, Reach::Nothing),
    d("workspace_read", EffectGroup::Other, Reach::Nothing),
    // `workspace_search` (issue #607) is a read of the same tree by the same
    // rules — it can surface nothing `workspace_read` could not already be asked
    // for, and it exists precisely so that asking costs one call instead of one
    // per candidate. Anything stricter would price the cheap path above the
    // expensive one it replaces.
    //
    // Note which grant it rides, because the name invites the wrong guess: the
    // `workspace` READ grant, never the metered `search` grant. `web_search`
    // spends money at a backend; this reads the company's own notes.
    d("workspace_search", EffectGroup::Other, Reach::Nothing),
    d("workspace_create", EffectGroup::Other, Reach::Consequence),
    d("workspace_write", EffectGroup::Other, Reach::Consequence),
    // `workspace_delete` and `workspace_rename` (issue #671) take the same
    // classification, and again by re-deriving it rather than by copying.
    //
    // Both are confined to the agent's own `agents/<self>/` folder, which is
    // narrower than either tool above — so the temptation is to price them
    // lower. That would be backwards. Reach here is about what a call costs the
    // company, and a delete removes a node **and its authorship record** from a
    // tree the operator and every teammate read; a rename moves what somebody
    // may have linked to by path. Neither is undone by the agent that did it,
    // and the operator's undo is a console session, not a retry. `Consequence`
    // is what "the operator would want to have seen this" means.
    //
    // `PerCall` follows for the same reason it does above, with one more: a
    // standing grant on deletion is precisely the shape that turns one bad turn
    // into a folder that is quietly empty by the end of it. Per-call parking
    // makes each removal its own card naming its own path.
    d("workspace_delete", EffectGroup::Other, Reach::Consequence),
    d("workspace_rename", EffectGroup::Other, Reach::Consequence),
    // ---- Agent-authored internal dashboard pages ---------------------------
    // Same re-derivation as `workspace_*` immediately above, not a copy: reads
    // are free, and a write or delete reaches past this turn because it lands
    // in the same shared `WorkspaceStore` tree every operator and teammate
    // reads (`pages/<slug>/`), and — once the operator opens the page — is
    // rendered live in the console. `pages_write` additionally compiles and
    // publishes a *runnable* artifact, which is strictly more externally
    // visible than overwriting a note, so it cannot be priced any lower than
    // `workspace_write`. `PerCall`, not `Grantable`, for the identical reason:
    // a standing grant on either would let one bad turn silently replace or
    // remove a page the operator has already put in front of the company.
    d("pages_list", EffectGroup::Other, Reach::Nothing),
    d("pages_read", EffectGroup::Other, Reach::Nothing),
    d("pages_write", EffectGroup::Other, Reach::Consequence),
    d("pages_delete", EffectGroup::Other, Reach::Consequence),
    // ---- Publishing --------------------------------------------------------
    // Externally visible and not reversible by the company alone.
    // `Reach::Consequence` because a publish does change state, and a
    // `supervised` desk should still see one before it lands. But `Grantable`,
    // not `PerCall` (issue #903): handing a finished file to the operator does
    // not leave the company. `harness::publish` writes into the company's own
    // workspace and artifact chain — no counterparty, no address, nothing sent
    // — and the write is versioned (`…/artifacts/{id}/versions`, `…/diff`), so
    // it is reversible by the company alone, unlike the tools this section's
    // neighbours classify. `auto` already promises that the agent's own
    // sandbox writes run unattended; this is that promise applied to the step
    // that makes the work visible. An operator who wants a human on every
    // hand-over keeps one by choosing `supervised`, or by naming
    // `publish_artifact` in `always_approve`, which wins over every tier.
    d_grantable("publish_artifact", EffectGroup::Publish, Reach::Consequence),
    // ---- Priced backend calls ----------------------------------------------
    // `web_search` is billed per request but changes nothing (issue #238).
    // Media generation moves real money on submit (issue #109); listing the
    // catalogue is a GET to the same backend that changes nothing and costs
    // nothing.
    d("web_search", EffectGroup::Spend, Reach::Money),
    d(
        "media_generate_image",
        EffectGroup::Spend,
        Reach::Consequence,
    ),
    d(
        "media_generate_video",
        EffectGroup::Spend,
        Reach::Consequence,
    ),
    d("media_list_models", EffectGroup::Other, Reach::Nothing),
    // ---- Skills catalogue --------------------------------------------------
    // The three OpenHuman skill *read* tools, scoped to this agent's own
    // materialized skill tree under its workspace. All local, all reads.
    //
    // `describe_workflow` PARKED before this table existed, for the same
    // reason `file_read` did and with nobody reporting either: the read-only
    // rule matched a name *prefix* and "describe" is not one of the words. The
    // persona hands an agent all three in one sentence — "use `list_skills`
    // to enumerate them, `describe_skill` to inspect one" — so two ran and
    // the middle one interrupted an operator.
    //
    // Issue #845 renamed all three off upstream's "workflow" wording, which is
    // a *skill* upstream and a saved graph here. `describe_skill` inherits
    // exactly the hazard above — "describe" is still not a read-only prefix —
    // so these three rows are what keep the rename from re-parking it.
    //
    // Spelled as literals, not as `harness::skills::naming`'s constants: this
    // table is compiled in every build and that module is behind `openhuman`.
    // `skill_read_tools_are_declared_reads` (in that module, where the constants
    // are) is what pins the two spellings together.
    d("list_skills", EffectGroup::Other, Reach::Nothing),
    d("describe_skill", EffectGroup::Other, Reach::Nothing),
    d("read_skill_resource", EffectGroup::Other, Reach::Nothing),
    // ---- MCP ---------------------------------------------------------------
    // The agent persona *instructs* every agent to call `mcp_list_tools` on a
    // named server rather than answer a capability question from memory, so
    // parking it made the guidance that exists to prevent stale answers cost
    // an operator approval to follow (issue #443).
    //
    // `mcp_registry_list_tools` reads process-local registration state,
    // credentials already redacted. `mcp_list_tools` is
    // NOT local — it is a `tools/list` round trip to the operator-configured
    // server. It is `Nothing` all the same: it changes nothing there or here
    // and is billed for nothing, and a desk that cannot ask a server what it
    // offers cannot use one. Whether a call *reaches* is the question this
    // module stopped asking; what it costs is the question it asks instead.
    //
    // Calling *through* a server is a consequence and stays per-call: it can
    // perform any effect the third-party server advertises.
    d("mcp_list_tools", EffectGroup::Other, Reach::Nothing),
    d(
        "mcp_registry_list_tools",
        EffectGroup::Other,
        Reach::Nothing,
    ),
    // `mcp_call_tool` keeps its row so `declared_tools` still walks it, but its
    // reach is decided from the (server, tool) pair the call carries against the
    // operator's per-server read declaration — see `mcp_call_tool_consequence`
    // (#1124). This row's `Consequence` is the answer for a call whose server or
    // tool argument cannot be read, and for a build without the classifier, both
    // of which that function falls back to.
    d(MCP_CALL_TOOL, EffectGroup::Other, Reach::Consequence),
    // Billing (issues #788, #789). Both integrations read the company's OWN
    // Chargebee site and PayPal account, so the reads are `Nothing` rather than
    // `ExternalRead`: that tier exists for reaching into a *counterparty's*
    // account, and a `readonly` desk answering "has Alan paid?" about the
    // company's own ledger changes nothing and bills nothing.
    d("chargebee_get_invoice", EffectGroup::Other, Reach::Nothing),
    d(
        "chargebee_list_invoices",
        EffectGroup::Other,
        Reach::Nothing,
    ),
    d("chargebee_get_customer", EffectGroup::Other, Reach::Nothing),
    d(
        "paypal_get_wallet_balance",
        EffectGroup::Other,
        Reach::Nothing,
    ),
    d(
        "paypal_list_transactions",
        EffectGroup::Other,
        Reach::Nothing,
    ),
    // Raising an invoice reaches a real customer of a real business and creates
    // a demand for money, so it is `Send` and it parks.
    d(
        "chargebee_send_invoice",
        EffectGroup::Send,
        Reach::Consequence,
    ),
    // Writes a record into an external billing system. No money moves, but it
    // is still a change somebody else's system will keep.
    d(
        "chargebee_create_customer",
        EffectGroup::Other,
        Reach::Consequence,
    ),
    // The registry twin of `mcp_call_tool`, graded the same way against the same
    // declaration (#1124). Its row is the fallback for an unreadable call.
    d(
        MCP_REGISTRY_TOOL_CALL,
        EffectGroup::Other,
        Reach::Consequence,
    ),
    // ---- Composio ----------------------------------------------------------
    // The three list tools are authenticated GETs to the managed backend with
    // the tenant's own bearer (issue #110) — over the wire, but changing
    // nothing and billed for nothing, so they run in every mode. Authorizing
    // begins an OAuth handoff that establishes an account identity for the
    // company, which is a change, so it parks.
    //
    // `composio_execute` is NOT here: one name carries every action, so its
    // consequence is read from the action slug in its arguments — see
    // `composio_execute_consequence`.
    d("composio_list_toolkits", EffectGroup::Other, Reach::Nothing),
    d(
        "composio_list_connections",
        EffectGroup::Other,
        Reach::Nothing,
    ),
    d("composio_list_tools", EffectGroup::Other, Reach::Nothing),
    d(
        "composio_authorize",
        EffectGroup::Identity,
        Reach::Consequence,
    ),
    // ---- Hosting (issues #1079, #913) --------------------------------------
    //
    // The ten `hosting_*` tools openhuman ships in its `hosting/` module.
    // Declared here rather than left to `undeclared()`, which is what that
    // fallback's own doc asks for: it is "a courtesy for an unregistered read,
    // not a second classifier to trust with an unreviewed capability".
    //
    // **Why the fallback gets these wrong, and why it is not being taught to
    // get them right.** `undeclared()` decides "is this a read?" with
    // `READ_ONLY_PREFIXES` matched by `name.starts_with(p)`. Every name here
    // begins with `hosting_`, so `list`/`get`/`read` can never match — the
    // prefix test cannot see past the namespace, and every one of them comes
    // back `Consequence`. Widening that test to look inside a namespace is the
    // tempting general fix and is the wrong one: the fallback runs ONLY for
    // tools no belt registered (a registered one is caught by
    // `every_registered_tool_is_declared`), so making it cleverer extends trust
    // to exactly the population that has had no review. It would also trade a
    // fail-CLOSED miss — a read that parks, which costs an approval — for a
    // fail-OPEN one, an effect that reads as a read. Declaring is the fix the
    // file already prescribes.
    //
    // Six reads, per openhuman's own `hosting/README.md`, which labels each of
    // them "Read-only.". They ask the provider what exists and what it did;
    // nothing leaves this company and nothing is spent.
    d(
        "hosting_deployment_status",
        EffectGroup::Other,
        Reach::Nothing,
    ),
    // `hosting_deployment_logs` arrived with the 2026-09 vendor bump, in
    // `tools/deployments.rs` beside the two above: "a deployment's build and
    // runtime log events, oldest first, trimmed to the most recent. Read-only."
    // It is the tool that says *why* a `hosting_deployment_status` failure
    // failed, so parking it would cost an approval on the way to every
    // diagnosis — the same reasoning `hosting_list_deployments` carries.
    d(
        "hosting_deployment_logs",
        EffectGroup::Other,
        Reach::Nothing,
    ),
    d("hosting_list_sites", EffectGroup::Other, Reach::Nothing),
    d("hosting_analytics", EffectGroup::Other, Reach::Nothing),
    // `hosting_list_deployments` is the history behind `hosting_deployment_status`
    // — "recent deployments, newest first, with their status, target and
    // creation time" — and carries the same label for the same reason. It is
    // also the tool that hands `hosting_rollback` its `deployment_id`, so
    // parking it would cost an approval on the way to every recovery.
    d(
        "hosting_list_deployments",
        EffectGroup::Other,
        Reach::Nothing,
    ),
    // `hosting_domain_status` is the read half of `hosting_add_domain`: it
    // reports whether a domain the company already attached has been verified
    // and is serving. Attaching is the effect; asking is not.
    d("hosting_domain_status", EffectGroup::Other, Reach::Nothing),
    // The public deployment itself: it spends money and changes what the world
    // sees at an address. `Publish` is the label an operator's card needs, and
    // the fallback gave it `Other` — its name contains no `deploy`, `publish` or
    // `post`, while the *status read* contains `deploy` and was labelled
    // `Publish`. The two were inverted, which is worse than both being vague.
    d(
        "hosting_launch_site",
        EffectGroup::Publish,
        Reach::Consequence,
    ),
    // Attaching a domain is the other half of "what the world sees at an
    // address", so it carries the same label as the launch it points at.
    d(
        "hosting_add_domain",
        EffectGroup::Publish,
        Reach::Consequence,
    ),
    // `hosting_set_env` is the one the issue left open, and the tool's own
    // description settles it: "The site must be redeployed afterwards for a
    // build-time variable to take effect." It changes what the NEXT deployment
    // serves; it does not itself deploy. `Publish` would tell an operator a
    // deployment is happening when none is, which is the same misdescription
    // this change exists to remove — so `Other`, and `Consequence` because it
    // still writes provider state and can store secrets write-only there.
    d("hosting_set_env", EffectGroup::Other, Reach::Consequence),
    // `hosting_rollback` is the tool the NOTE here used to hold a place for
    // (issue #913). It arrived with the vendor pin, and the shape that note
    // predicted is the one the tool asks for: it "points production traffic at
    // an earlier deployment", and openhuman marks it `external_effect()` with
    // the comment "Changes what the public sees on a live site, so it gates."
    // That is `hosting_launch_site`'s sentence with the build removed, so it
    // takes `hosting_launch_site`'s label — what the world sees at an address
    // changes either way, and an operator's card should say so.
    //
    // It parks, and that is deliberate even though rollback is the *recovery*
    // path and parking it delays a fix to a broken site. `supervised` exists to
    // put a human in front of a change to what the public sees, and "the site
    // is already broken" is an argument for approving quickly, not for not
    // being asked. An operator who wants it unattended has `always_approve`.
    d("hosting_rollback", EffectGroup::Publish, Reach::Consequence),
];

/// A per-call declaration — the default. `const fn` so [`DECLARED`] stays a
/// `const` the compiler can lay out statically.
const fn d(tool: &'static str, group: EffectGroup, reach: Reach) -> Declared {
    Declared {
        tool,
        group,
        reach,
        standing: Standing::PerCall,
    }
}

/// A declaration an operator may grant standing on.
const fn d_grantable(tool: &'static str, group: EffectGroup, reach: Reach) -> Declared {
    Declared {
        tool,
        group,
        reach,
        standing: Standing::Grantable,
    }
}

/// One tool's argument classifier: the whole of what it needs is the call's
/// arguments, so every entry of [`ARGUMENT_GRADED`] has this one shape.
type Grader = fn(&serde_json::Value) -> Consequence;

/// The tools whose consequence is a property of their **arguments**, not of
/// their name — as data, so the set is enumerable rather than inferred from
/// control flow (issue #877).
///
/// Issue #877 states the criterion this exists to meet: *"the coverage test
/// keeps saying which tools answer from arguments and which from the table, so
/// a new tool cannot quietly join the coarse side."* Before this, the four
/// classifiers were hand-written `if` arms in [`consequence_of`] and
/// [`declared_tools`] chained exactly one name — `composio_execute` — by hand.
/// A fifth classifier could therefore be added, dispatched, and still be
/// invisible to every test that walks [`declared_tools`], because nothing tied
/// the two together. Here they are the same list.
///
/// The roster is consulted **before** [`DECLARED`], so an entry that also holds
/// a table row shadows it. That is deliberate and the table rows stay: a row is
/// the answer for a call whose arguments cannot be read, and it keeps the tool
/// visible to every reader who walks [`DECLARED`] looking for what a tool can
/// reach. `composio_execute` is the one entry with no row, which is why
/// [`declared_tools`] has to union rather than concatenate.
///
/// Ordered for reading:
///
/// * `composio_execute` — #441, keyed on the action slug.
/// * `web_fetch` — keyed on the URL's host.
/// * `http_request` — keyed on its method, URL host, and the rest of the
///   request shape: a body or a non-allowlisted header gates it even when the
///   method reads GET/HEAD/OPTIONS (see [`http_request_consequence`]). `curl`
///   is deliberately NOT here: unlike `web_fetch`/`http_request`, it always
///   writes the response to the workspace `downloads/` dir, so it stays on
///   the [`DECLARED`] row's `Reach::Consequence`.
/// * `shell` — #875, keyed on the command line.
/// * `git_operations` — #877, keyed on the `operation`.
/// * `mcp_call_tool` / `mcp_registry_tool_call` — #1124, keyed on the
///   (server, tool) pair, but *only downgraded* against a per-server read
///   declaration the operator supplies — which is company context this pure
///   function cannot see. So the roster entry answers the fail-closed base
///   (`Reach::Consequence`), and the downgrade is applied by
///   [`mcp_call_reach`], which the policy calls with the declaration in hand.
/// * `workspace_create` / `workspace_write` / `workspace_delete` /
///   `workspace_rename` — #877, keyed on the resolved node's durable
///   authorship. The company-scoped lookup lives at the policy seam, so this
///   pure classifier deliberately returns the fail-closed table verdict.
///
/// Every name here must be **lower-case**: [`consequence_of`] matches against a
/// lower-cased tool name, so a mixed-case entry would be an entry that never
/// fires. `the_roster_is_lower_case_and_has_no_duplicates` holds that.
const ARGUMENT_GRADED: &[(&str, Grader)] = &[
    (COMPOSIO_EXECUTE, composio_execute_consequence),
    (WEB_FETCH, web_fetch_consequence),
    ("http_request", http_request_consequence),
    (SHELL, shell_consequence),
    (GIT_OPERATIONS, git_operations_consequence),
    (MCP_CALL_TOOL, mcp_call_tool_consequence),
    (MCP_REGISTRY_TOOL_CALL, mcp_call_tool_consequence),
    ("workspace_create", workspace_mutation_consequence),
    ("workspace_write", workspace_mutation_consequence),
    ("workspace_delete", workspace_mutation_consequence),
    ("workspace_rename", workspace_mutation_consequence),
];

/// The classifier that answers for `name`, or `None` when the table does.
///
/// `name` is expected already lower-cased, as [`consequence_of`] lower-cases
/// once and then asks both mechanisms.
fn argument_grader(name: &str) -> Option<Grader> {
    ARGUMENT_GRADED
        .iter()
        .find(|(tool, _)| *tool == name)
        .map(|(_, grade)| *grade)
}

/// Every tool name the gate classifies, for the coverage test.
///
/// The union of [`DECLARED`] and [`ARGUMENT_GRADED`] — the two mechanisms
/// together — with the roster's shadowed rows counted once. Derived rather
/// than hand-maintained so a new argument classifier joins the coverage test by
/// joining the roster, which is the whole of the mechanism it takes to dispatch
/// it (issue #877).
pub fn declared_tools() -> impl Iterator<Item = &'static str> {
    tool_names(DECLARED, ARGUMENT_GRADED)
}

/// [`declared_tools`] over an explicit pair of tables.
///
/// Split out so a test can drive the derivation with a *synthetic* roster and
/// show that an argument-graded tool with no [`DECLARED`] row is still
/// enumerated. That is the property #877 asks for and the one the previous
/// hand-written `chain(once(COMPOSIO_EXECUTE))` could not have: it named the
/// single exception rather than deriving it.
fn tool_names(
    declared: &'static [Declared],
    graded: &'static [(&'static str, Grader)],
) -> impl Iterator<Item = &'static str> {
    declared.iter().map(|d| d.tool).chain(
        graded
            .iter()
            .map(|(tool, _)| *tool)
            .filter(move |tool| !declared.iter().any(|d| d.tool == *tool)),
    )
}

/// What this tool call can reach, and what an operator may do about it.
///
/// `args` are consulted, not decoration: `composio_execute` carries every
/// Composio action under one name, so classifying it from the name alone
/// collapsed a repository read and an outgoing email into the same verdict —
/// and the cautious answer had to win for both (issue #441). Additional tools
/// have since joined it, and they are listed in [`ARGUMENT_GRADED`] rather than
/// branched on here, so that the set of them can be tested rather than read off
/// this function's body.
pub fn consequence_of(tool: &str, args: &serde_json::Value) -> Consequence {
    let name = tool.to_ascii_lowercase();
    if let Some(grade) = argument_grader(&name) {
        return grade(args);
    }
    match DECLARED.iter().find(|d| d.tool == name) {
        Some(found) => Consequence {
            group: found.group,
            reach: found.reach,
            standing: found.standing,
        },
        None => undeclared(&name),
    }
}

/// Why a tool whose *name* reads like a read is nonetheless gated (issue #459).
///
/// `None` for almost everything, and that is right: "'shell' mutates or reaches
/// outside" needs no elaboration, and neither does `http_request`. The entries
/// here are the tools where the classification contradicts the name, so the
/// denial is the one an operator reads twice — `readonly` refusing something
/// called `read_*` looks like a bug in the tier, and without a reason the
/// operator has no way to tell it from one.
///
/// Appended to the `readonly` denial, which is where a confused operator ends
/// up: the `supervised` park explains itself by offering a card to approve.
pub fn denial_reason(tool: &str) -> Option<&'static str> {
    match tool.to_ascii_lowercase().as_str() {
        "read_workspace_state" => Some(
            "it runs `git status` and `git log` in the agent's workspace, and git \
             takes its configuration from that same directory — so it is gated \
             like `shell` rather than like a read",
        ),
        _ => None,
    }
}

/// The consequence of running one Composio action (issue #441).
///
/// ## Why the action and not the tool name
///
/// Every Composio action — listing a repository's pull requests, searching a
/// mailbox, sending an email, opening a PR — arrives as one tool,
/// `composio_execute`, with the action slug in the arguments. Classifying the
/// *name* meant the whole surface inherited the send verdict the sends deserve,
/// so no Composio read could ever hold a standing grant and an operator paid an
/// approval for every page of every list.
///
/// ## Where the read/send answer comes from
///
/// The provider's own curated catalogue, vendored with openhuman: ~660
/// hand-classified actions across ~30 toolkits, each tagged `Read` / `Write` /
/// `Admin`, already used upstream to enforce a read-only sandbox. It is a
/// pure, synchronous, in-process table — no network on the approval path — and
/// it is the same source the provider surfaces the actions from, so it does not
/// drift the way a list maintained here would the moment a toolkit gains an
/// action.
///
/// ## What the catalogue does not name is read for its verb (issue #1818)
///
/// The catalogue is ~660 hand-classified actions; Composio publishes thousands
/// and renames them without asking. So the miss is not the exception it reads
/// as: an agent fetching a repository's issues under a live slug that is one
/// rename away from the curated one used to land on a blanket
/// `Send + Consequence + PerCall` — a park whose card says *leaves the company
/// or spends money* for what is a read, and which `PerCall` makes impossible to
/// grant standing on, so the desk stops. Catalogue drift, not agent behaviour,
/// was deciding whether a company could read its own GitHub issues.
///
/// A miss now falls to [`composio_slug_reads_by_verb`], which asks what the
/// slug's own verb says: a read verb present and no mutating verb anywhere.
/// `..._LIST_...`, `..._GET_...`, `..._FETCH_...`, `..._SEARCH_...` are reads
/// and classify as [`Reach::ExternalRead`] — `readonly` still denies them,
/// `supervised` and `auto` run them. Everything else, including a compound like
/// `..._GET_AND_UPDATE_...`, is a send exactly as before.
///
/// This is deliberately **not** upstream's `classify_unknown`, whose fallback
/// arm returns `Read` for any slug carrying no *write* verb — that hands the
/// read verdict to every slug nobody has classified, including the ones whose
/// verbs mean nothing to us. The rule here is the opposite polarity: a read
/// needs positive evidence, and its absence is still a send.
/// `we_do_not_fall_back_to_the_upstream_read_default` pins the difference on
/// `GITHUB_INVENT_A_NEW_VERB`, which upstream calls a read and this calls a
/// send.
///
/// ## An inferred read is never grantable
///
/// A catalogued read is [`Standing::Grantable`]; an inferred one is
/// [`Standing::PerCall`]. Both run unattended — [`Reach::ExternalRead`] does
/// not park, so [`Consequence::parks_under_auto`] is `false` either way and the
/// stall this issue is about is gone. What `PerCall` withholds is the **mint**:
/// no standing grant may be cut from a verb guess, because a grant outlives the
/// call it was minted for and a guess should not. The narrow reading is the one
/// that expires with the turn.
///
/// The other cautious paths are untouched: a missing or non-string `tool`
/// argument has no verb to read and stays a send, and so does every slug in a
/// build with no catalogue compiled in — see [`CatalogLookup::CatalogueAbsent`].
///
/// ## Two different reasons for the same verdict
///
/// "The catalogue has never heard of this slug" and "these arguments carry no
/// slug at all" both end in a send, and that is right — but only one of them is
/// a caller bug. Issue #470 survived for as long as it did precisely because
/// the two were indistinguishable from outside: fixtures named their action
/// under a key nothing reads, every call fell through to the fallback, and the
/// verdicts still looked plausible. The verdict stays cautious either way; the
/// second case now says so in the log, via [`ActionKeyMiss`], so a caller
/// building the wrong argument shape is visible rather than silently safe.
fn composio_execute_consequence(args: &serde_json::Value) -> Consequence {
    let send = Consequence {
        group: EffectGroup::Send,
        reach: Reach::Consequence,
        standing: Standing::PerCall,
    };
    let slug = match composio_action_slug(args) {
        Ok(slug) => slug,
        Err(miss) => {
            tracing::warn!(
                "[policy] a '{COMPOSIO_EXECUTE}' call carries no readable \
                 '{COMPOSIO_ACTION_KEY}' argument ({}); classifying it as a send, which is \
                 the cautious answer but not the one the catalogue would have given — the \
                 caller is building an argument shape the tool's own schema rejects",
                miss.describe()
            );
            return send;
        }
    };
    let lookup = composio_catalog_lookup(slug);
    // Issue #754: a catalogue miss is recorded, not silent. A drifted read and
    // a correct refusal used to be the same `send` at the approval card, so
    // without this nobody learns the curated names and Composio's live names
    // have moved apart. The slug and toolkit are the dataset any future alias
    // mapping would be designed from.
    //
    // Issue #1818 gave the miss a second job: it is also the point where the
    // verb fallback decides, so each line below now says which way it went. The
    // miss is still worth logging when the fallback rescues it — a rescued read
    // is drift that has already cost nothing, and it is the clearest possible
    // evidence of *which* curated name went stale.
    //
    // Deliberately NOT emitted for a curated write: that is the gate working,
    // and logging it would bury the real signal under every `GMAIL_SEND_EMAIL`.
    let inferred_read = match &lookup {
        CatalogLookup::Curated { .. } => return curated(lookup.is_read()),
        CatalogLookup::UncuratedAction { toolkit } => {
            let reads = composio_slug_reads_by_verb(slug);
            tracing::warn!(
                composio_slug = %slug,
                composio_toolkit = %toolkit,
                catalogue_miss = true,
                inferred_read = reads,
                "[policy] '{slug}' is not in the '{toolkit}' curated catalogue; its own verb \
                 says it {} (issues #754, #1818). A miss here is what a catalogued read looks \
                 like once its live slug has drifted from the curated one, so the curated name \
                 is the thing to fix.",
                if reads { "only reads, so it runs as an external read" } else { "is a send" }
            );
            reads
        }
        CatalogLookup::UnknownToolkit { toolkit } => {
            let reads = composio_slug_reads_by_verb(slug);
            tracing::warn!(
                composio_slug = %slug,
                composio_toolkit = toolkit.as_deref().unwrap_or("<unrecognised>"),
                catalogue_miss = true,
                inferred_read = reads,
                "[policy] no curated catalogue to classify '{slug}' against; its own verb says \
                 it {} (issues #754, #1818).",
                if reads { "only reads, so it runs as an external read" } else { "is a send" }
            );
            reads
        }
        // The build seam, said out loud rather than left to look like drift
        // (issue #1818). Without the catalogue linked in *every* Composio
        // action over-gates, including ones the curated table names — so this
        // is a fact about the binary, not about the slug, and the verb
        // fallback is deliberately not consulted: a build that cannot tell a
        // curated read from an uncurated one has no business inferring either.
        CatalogLookup::CatalogueAbsent => {
            catalogue_absent_warning();
            tracing::warn!(
                composio_slug = %slug,
                catalogue_absent = true,
                "[policy] '{slug}' is classified as a send because this build links no curated \
                 Composio catalogue, not because the action sends (issue #1818)."
            );
            return send;
        }
    };
    if inferred_read {
        // Same reach as a catalogued read — it does not park, so the desk keeps
        // moving — but `PerCall`, because a verb is evidence and not a
        // classification. See "An inferred read is never grantable" above.
        return Consequence {
            group: EffectGroup::Other,
            reach: Reach::ExternalRead,
            standing: Standing::PerCall,
        };
    }
    send
}

/// The verdict for a slug the curated catalogue *does* name.
///
/// Split out so the catalogued read keeps its own argument — which is about a
/// hand-assigned scope — separate from the inferred read's, which is about a
/// verb. They agree on [`Reach`] and differ on [`Standing`], and that is the
/// whole of the distinction issue #1818 introduces.
fn curated(read: bool) -> Consequence {
    if read {
        // A read reaches a third-party account, so `readonly` denies it — but
        // it changes nothing and is billed for nothing, so `supervised` lets it
        // through (issue #559).
        //
        // It used to be `Reach::Consequence`, which parks. The intent was that
        // the operator consent once and grant a standing scope; the effect was
        // that checking a mailbox interrupted a person, refused the call and
        // dead-ended the turn — per page, per list. A first-time park is not a
        // cheap price for a read, it is the whole cost.
        //
        // `Standing::Grantable` stays — but not for the reason the issue gives.
        // It does **not** govern `readonly`: that brake denies off
        // `Reach::denied_under_readonly` before any grant is consulted
        // (`harness::policy::check`, "readonly outranks a grant"), and it never
        // reads `Standing` at all. Under `supervised` nothing parks here now,
        // so the admission path this used to feed is unreachable for a
        // catalogue read in every tier that exists today.
        //
        // What it still governs is the **mint** side: a standing grant may only
        // be minted for a grantable call
        // (`Effect::may_be_granted_standing`, and the re-check in
        // `standing_grant_allows`), and a Composio *send* arriving under this
        // same tool name must never be mintable. It is also the field any
        // unattended tier has to read to tell a read it may run from a send it
        // may not — the `auto` tier proposed in #560 derives exactly that line.
        // Removing it would make both of those decisions unrepresentable.
        Consequence {
            group: EffectGroup::Other,
            reach: Reach::ExternalRead,
            standing: Standing::Grantable,
        }
    } else {
        Consequence {
            group: EffectGroup::Send,
            reach: Reach::Consequence,
            standing: Standing::PerCall,
        }
    }
}

/// What the curated catalogue knows about one action slug (issue #754).
///
/// The classification is unchanged by this type — everything that is not a
/// curated read is still a send. It exists so a **catalogue miss** stops being
/// silent: today a drifted read and a genuine send are the same `false`, so a
/// stale catalogue is indistinguishable from a correct refusal and nobody ever
/// learns the names have moved.
///
/// The distinction that matters is [`Curated`](Self::Curated) `{ read: false }`
/// versus [`UncuratedAction`](Self::UncuratedAction). A curated write is the
/// gate working; an uncurated slug is the gate guessing. Logging both would
/// bury the second in the first — every `GMAIL_SEND_EMAIL` would look like
/// drift.
///
/// The first three arms are constructed only by the `openhuman` build of
/// [`composio_catalog_lookup`]; without that feature the curated catalogue is
/// not linked in, so the only reachable answer is
/// [`CatalogueAbsent`](Self::CatalogueAbsent) and the others are dead there.
/// The type stays whole across both builds on purpose — the call site matches
/// one shape, and `is_read` keeps one definition, so the feature cannot change
/// what classifies as a read. The expectations are `expect` rather than `allow`
/// and each is scoped to the build that earns it: if the build that cannot
/// construct an arm ever does, the unfulfilled expectation says so instead of
/// staying quietly stale.
///
/// [`CatalogueAbsent`](Self::CatalogueAbsent) split off from
/// `UnknownToolkit { toolkit: None }` for issue #1818. They were the same value
/// and they are not the same fact: one is a slug this build could not place,
/// the other is a build that can place nothing. Only the first is a candidate
/// for the verb fallback, and only the second is worth telling an operator
/// about — "every Composio action over-gates in this binary" is a deployment
/// bug, and it used to be indistinguishable from an unrecognised toolkit.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(
    not(feature = "openhuman"),
    expect(
        dead_code,
        reason = "without the harness feature there is no catalogue to hit, so only \
                  CatalogueAbsent is constructible; see the note above"
    )
)]
pub(crate) enum CatalogLookup {
    /// The slug is in its toolkit's catalogue, with a hand-assigned scope.
    Curated { read: bool },
    /// The toolkit has a catalogue and this slug is not in it — the drift case
    /// #754 is about, and the one worth recording.
    UncuratedAction { toolkit: String },
    /// A catalogue was consulted and had nothing to say: the slug named no
    /// toolkit this build knows (`None`), or the toolkit it named has no
    /// curated surface at all.
    UnknownToolkit { toolkit: Option<String> },
    /// There is no catalogue in this build to consult — the `openhuman`
    /// feature is off, so nothing can be classified and every Composio action
    /// is a send by construction rather than by verdict (issue #1818).
    // `not(test)` because the harness build's own tests *do* construct it —
    // `a_catalogued_build_never_reports_the_catalogue_absent` asserts nothing
    // ever equals it — so the lint fires only in the build where no test does.
    #[cfg_attr(
        all(feature = "openhuman", not(test)),
        expect(
            dead_code,
            reason = "with the harness feature the catalogue is always linked in, so this \
                      arm is matched but never constructed; see the note above"
        )
    )]
    CatalogueAbsent,
}

impl CatalogLookup {
    /// The verdict this feeds — byte-identical to the boolean it replaced.
    fn is_read(&self) -> bool {
        matches!(self, Self::Curated { read: true })
    }
}

/// Why a `composio_execute` call carries no action slug this classifier can
/// read (issue #470).
///
/// Every variant classifies as a send, so this changes no verdict. It exists so
/// the log line can say *which* shape arrived: a caller that omits the key, one
/// that sends a number where a slug belongs, and one naming an action the
/// catalogue has never heard of are three different mistakes, and only the last
/// is a legitimate call to an unclassified action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ActionKeyMiss {
    /// The arguments are not a JSON object at all.
    NotAnObject,
    /// An object, but with no [`COMPOSIO_ACTION_KEY`] property — the shape the
    /// `tool_slug` fixtures of #470 had.
    KeyAbsent,
    /// The key is present but not a string.
    NotAString,
    /// The key is present and a string, but empty, so no lookup can succeed.
    Empty,
}

impl ActionKeyMiss {
    /// A short phrase for the log line, in the caller's terms.
    pub(crate) fn describe(self) -> &'static str {
        match self {
            Self::NotAnObject => "the arguments are not an object",
            Self::KeyAbsent => "the key is absent",
            Self::NotAString => "the key is present but not a string",
            Self::Empty => "the key is present but empty",
        }
    }
}

/// The action slug in a `composio_execute` call's arguments, or why there
/// isn't one.
pub(crate) fn composio_action_slug(args: &serde_json::Value) -> Result<&str, ActionKeyMiss> {
    let Some(object) = args.as_object() else {
        return Err(ActionKeyMiss::NotAnObject);
    };
    let Some(value) = object.get(COMPOSIO_ACTION_KEY) else {
        return Err(ActionKeyMiss::KeyAbsent);
    };
    let Some(slug) = value.as_str() else {
        return Err(ActionKeyMiss::NotAString);
    };
    if slug.trim().is_empty() {
        return Err(ActionKeyMiss::Empty);
    }
    Ok(slug)
}

/// Which slice of a tool one standing grant is confined to (issue #457).
///
/// `None` for almost everything, and that is the honest answer: for a tool
/// whose name *is* the whole of what it can do — `file_write`, `memory_store` —
/// "this tool, for this teammate, until a deadline" already describes exactly
/// what the operator consented to, and there is nothing left to narrow.
///
/// `composio_execute` is the exception the type exists for. Every Composio
/// action across every connected toolkit arrives under that one name, so a grant
/// keyed on the name alone turns "read from GitHub" — the sentence on the card —
/// into "make any Composio read, anywhere". [`consequence_of`] already
/// re-classifies the live action, so a send cannot slip through a read's grant;
/// what it cannot see is that a *different provider's* read is a different
/// sentence. The toolkit is that dimension, and it is the right grain: the
/// operator agreed to a provider, not to one action slug, so a second GitHub
/// read must still pass.
///
/// Read through the vendored catalogue, so a toolkit nobody has classified
/// resolves to `None` and — per [`StandingGrant::admits_scope`] — a scoped grant
/// refuses to admit it. Without the harness feature this is always `None`, which
/// is safe rather than lax: that build cannot mint a Composio standing grant in
/// the first place — `without_the_catalogue_every_composio_action_is_a_send`
/// pins that — so there is no scoped grant there for `None` to widen.
///
/// # No live caller, and retained on purpose (issue #610)
///
/// **Nothing in any current tier routes a Composio call to this.** Since #559 a
/// catalogue read no longer parks under `supervised`, so the tier allows it well
/// above the grant checks; under `readonly` the #243 emergency brake denies
/// every external effect *above* them; under `full` everything is allowed. The
/// scope is minted and stored, and no tier spends it.
///
/// That is dormancy, not death, and the distinction is written here because a
/// mechanism with no caller and no note is indistinguishable from dead code —
/// and this one is a reviewed security boundary. It is what any future durable
/// allowlist would consult, and rebuilding it later would mean re-deriving the
/// same decision: that consent attaches to a *provider*, that a slug the
/// catalogue cannot place resolves to `None`, and that a scoped grant refuses
/// `None` rather than guessing permissively. #563 proposed one such feature and
/// was closed because its **card-promotion** shape cannot work under `auto`: a
/// tool parks under `auto` exactly when it is not [`Standing::Grantable`], and
/// the standing-grant control is offered only for `Grantable` tools, so no card
/// under `auto` can ever offer "don't ask again" — exact complements on the
/// parked set, a partition rather than a gap. A console- or manifest-managed
/// allowlist consulted **before** the tier check has no such problem, and would
/// use this.
///
/// Deleting it is therefore a decision to re-derive it later, and should be made
/// as one. `the_minted_scope_is_the_scope_a_grant_admits` pins this function
/// against [`StandingGrant::admits_scope`] directly, so the pairing keeps its
/// coverage while no caller connects them.
///
/// [`Standing::Grantable`]: crate::policy::consequence::Standing::Grantable
/// [`StandingGrant::admits_scope`]: crate::runtime::grants::StandingGrant::admits_scope
/// The `scheme://host[:port]` a [`WEB_FETCH`] call addresses, or `None` when the
/// argument cannot be read as an absolute `http(s)` URL (issue #673).
///
/// This is the grant scope *and* the grantability test — see
/// [`web_fetch_consequence`]. `None` is therefore never a widening: it drops the
/// call back to [`Standing::PerCall`], which parks.
///
/// # Parsed by `url`, which is the parser that performs the fetch
///
/// The key is derived with [`url::Url`] rather than by reading the string here.
/// That is a security property, not a convenience: `reqwest` — and therefore the
/// vendored `web_fetch` — resolves the host with this same crate, so deriving the
/// grant key any other way means two parsers deciding what "the host" is, and
/// **every disagreement between them is a bypass**.
///
/// This is not hypothetical. The hand-rolled reader this replaced split the
/// authority on `/`, `?` and `#` only. Per WHATWG, `\` is also a path separator
/// in an http(s) URL, so `https://evil.com\@docs.rs/` is fetched from
/// `evil.com` — while that reader saw the authority as `evil.com\@docs.rs`,
/// took everything after the last `@`, and minted a grant for **`docs.rs`**. An
/// operator approving "fetch from docs.rs" would have authorised `evil.com`.
/// Tab, newline and carriage return are stripped by the URL parser before
/// parsing and were a second family of the same bug.
///
/// # What is in the key, and why
///
/// **The scheme**, so a grant approved for `https://docs.rs` cannot be spent on
/// `http://docs.rs`. The operator consented to a fetch that could not be read or
/// rewritten in transit, and silently honouring the cleartext twin would hand
/// back the guarantee they were shown.
///
/// **The port only when it is not the scheme's default**, which is
/// [`Url::port`]'s own normalization — so `https://docs.rs:443` and
/// `https://docs.rs` are one scope, as they are one service. A non-default port
/// stays in the key because `example.com:8443` is a different service.
///
/// **The host as `url` normalizes it** — lowercased, IDNA-encoded, IPv6 in
/// brackets. Matching is exact: no suffix, so a grant for `docs.rs` cannot admit
/// `evil-docs.rs`, and no subdomain, so it cannot admit `evil.docs.rs`. Both are
/// hosts the operator never read on the card.
///
/// Credentials are discarded, because [`Url::host_str`] returns the host and
/// never the userinfo — `https://docs.rs@evil.example/` is `evil.example`. A
/// URL that does not parse, names no host, or carries a non-http(s) scheme
/// resolves to `None`, which parks.
fn web_fetch_scope_of(args: &serde_json::Value) -> Option<String> {
    let raw = args.get(WEB_FETCH_URL_KEY)?.as_str()?;
    let parsed = url::Url::parse(raw.trim()).ok()?;
    let scheme = parsed.scheme();
    if scheme != "http" && scheme != "https" {
        return None;
    }
    let host = parsed.host_str()?;
    if host.is_empty() {
        return None;
    }
    Some(match parsed.port() {
        Some(port) => format!("{scheme}://{host}:{port}"),
        None => format!("{scheme}://{host}"),
    })
}

/// The consequence of one [`WEB_FETCH`] call (issue #673).
///
/// Argument-classified, exactly as `composio_execute` is and for the same
/// reason: what an operator can consent to is a property of *this call's*
/// arguments, not of the tool's name. "Fetch from `docs.rs` for the next few
/// days" is a sentence; "make any HTTP request" is not.
///
/// Both fields turn on the same read: whether [`web_fetch_scope_of`] can name a
/// concrete host.
///
/// * A resolvable host is [`Reach::ExternalRead`] (free under `supervised` and
///   `auto`) and [`Standing::ScopedGrantable`] — a card the operator actually
///   read, for a destination they actually saw.
/// * An unreadable URL — no `url` key, an unparseable string, or an unresolved
///   workflow expression such as `=item.endpoint` — falls back to
///   [`Reach::Consequence`] and [`Standing::PerCall`], the same gated shape
///   [`DECLARED`] gives the tool by default. "Free" is earned by a destination
///   the operator can see, not by a call merely shaped like a read; a call
///   whose destination is chosen at run time by an upstream value is exactly
///   the dynamic-destination case the workflow gate's own
///   `an_unresolved_url_gates_and_says_the_destination_is_not_known_yet` pins.
///
/// The standing half is also load-bearing beyond that: a grant is minted with
/// the scope [`standing_scope_of`] returns, and
/// [`StandingGrant::admits_scope`](crate::runtime::grants::StandingGrant::admits_scope)
/// treats an unscoped grant as admitting **everything**. Were an unreadable URL
/// still grantable, approving one card would mint a grant admitting every host
/// on earth. Tying both answers to one read makes that unrepresentable.
fn web_fetch_consequence(args: &serde_json::Value) -> Consequence {
    match web_fetch_scope_of(args) {
        Some(_) => Consequence {
            group: EffectGroup::Other,
            reach: Reach::ExternalRead,
            standing: Standing::ScopedGrantable,
        },
        None => Consequence {
            group: EffectGroup::Other,
            reach: Reach::Consequence,
            standing: Standing::PerCall,
        },
    }
}

/// Header names an `http_request` classified [`Reach::ExternalRead`] is
/// allowed to carry (PR #1989 review 3905098660).
///
/// An **allowlist**, not a denylist of override-style names such as
/// `X-HTTP-Method-Override` — a blocklist only catches spellings someone
/// thought to list (`X-Method-Override`, `X-HTTP-Method`, `X-Original-Method`,
/// …), which is the reactive pattern this codebase keeps getting burned by.
/// Every name below only shapes how the response is negotiated, cached, or
/// authenticated; none of them is a channel a server-side routing layer reads
/// as an instruction to treat the request as something other than the method
/// actually sent.
const HTTP_READ_SAFE_HEADERS: &[&str] = &[
    "accept",
    "accept-charset",
    "accept-encoding",
    "accept-language",
    "authorization",
    "cache-control",
    "if-match",
    "if-modified-since",
    "if-none-match",
    "if-unmodified-since",
    "range",
    "user-agent",
];

/// Whether every header key on an `http_request` call is in
/// [`HTTP_READ_SAFE_HEADERS`]. No `headers` key, or an explicit `null`,
/// passes trivially. A `headers` value that is not a JSON object cannot be
/// enumerated, so it fails closed rather than being read as empty.
fn http_request_headers_are_read_safe(args: &serde_json::Value) -> bool {
    match args.get("headers") {
        None | Some(serde_json::Value::Null) => true,
        Some(serde_json::Value::Object(map)) => map
            .keys()
            .all(|key| HTTP_READ_SAFE_HEADERS.contains(&key.to_ascii_lowercase().as_str())),
        Some(_) => false,
    }
}

/// Whether an `http_request` call carries no `body`. Any present, non-null
/// value — including an empty string — disqualifies the call: `to_tool_args`
/// forwards it verbatim and `HttpRequestTool::execute_request` attaches
/// whatever is forwarded to the outgoing request regardless of method, so a
/// body is a payload the request actually carries.
fn http_request_body_is_absent(args: &serde_json::Value) -> bool {
    matches!(args.get("body"), None | Some(serde_json::Value::Null))
}

/// The consequence of one `http_request` call, keyed on its method, URL host,
/// **and** the rest of the request shape (PR #1989 review 3905098660).
///
/// The method alone is not "read-only": [`crate::workflows::caps::http::to_tool_args`]
/// forwards `body` and `headers` unconditionally, and the wired
/// `HttpRequestTool::execute_request` attaches both to the outgoing request
/// without consulting the method — reqwest does not refuse a body on GET, and
/// a header such as `X-HTTP-Method-Override` is read by many server frameworks
/// as the *real* verb regardless of what was actually sent. Grading a `GET`
/// carrying either as [`Reach::ExternalRead`] would let `supervised`/`auto`
/// wave through an outbound data transmission (a GET body) or a server-side
/// mutation (a method-override header) with no approval card, defeating the
/// point of gating writes at all.
///
/// So `ExternalRead` requires the **whole** shape to be read-only: a
/// GET/HEAD/OPTIONS method, no `body`
/// ([`http_request_body_is_absent`]), and headers drawn only from
/// [`HTTP_READ_SAFE_HEADERS`] ([`http_request_headers_are_read_safe`]).
/// Anything else — a mutating method, an unrecognized method, or a read-shaped
/// method with a body or a non-allowlisted header — stays
/// `Reach::Consequence`/`Standing::PerCall`, the same fail-closed shape
/// [`DECLARED`] gives the tool by default.
fn http_request_consequence(args: &serde_json::Value) -> Consequence {
    let gated = Consequence {
        group: EffectGroup::Other,
        reach: Reach::Consequence,
        standing: Standing::PerCall,
    };
    let method = match args.get("method") {
        None => "GET",
        Some(value) => match value.as_str() {
            Some(method) => method,
            None => return gated,
        },
    };
    if !matches!(
        method.to_ascii_uppercase().as_str(),
        "GET" | "HEAD" | "OPTIONS"
    ) {
        return gated;
    }
    if !http_request_body_is_absent(args) || !http_request_headers_are_read_safe(args) {
        return gated;
    }
    web_fetch_consequence(args)
}

/// The consequence of running one shell command (issue #875).
///
/// ## Why the command and not the tool name
///
/// `shell` is how an agent looks at its own workspace. Classifying the name
/// meant `grep -c foo *.log` and `rm -rf /` were the same input to this
/// function, so an agent that investigates by grepping bought the operator an
/// approval card per command — under `supervised` and under `auto`, whose whole
/// contract is that it stops only before what leaves the company or spends
/// money. A `grep` in the agent's own workspace does neither.
///
/// ## Where the read/act answer comes from
///
/// The vendored runtime's own classifier, [`SecurityPolicy::classify_command`],
/// which OpenHuman gates its `ShellTool` with on the desktop product. It splits
/// the command into unquoted segments, classifies each against a curated
/// safe-read allowlist, and takes the **maximum** — so `grep x && rm -rf /` is
/// `Destructive`, not `Read` — then lifts anything with a redirect or `tee` to
/// `Write`. Anything it does not recognise is `Write`, which is the cautious
/// direction. [`shell_argv_read_exception`] then admits three omitted bases
/// only when their parsed argv proves that their writing forms are absent.
///
/// Reused rather than restated. A second list here would be a second thing to
/// keep current, and the moment the two disagreed the safer one would not
/// reliably be ours.
///
/// ## Only `Read` is downgraded
///
/// Every other class keeps exactly today's verdict — `Reach::Consequence` and
/// `Standing::PerCall`, so it parks under `supervised` and `auto` and can hold
/// no standing grant. A build without the harness feature has no classifier
/// linked in and gates everything, the same seam the Composio catalogue
/// straddles and answered the same way.
///
/// ## A read must also stay lexically inside the agent's own directory
///
/// `classify_command` grades by command *name*, never by path — `cat
/// /etc/passwd` and `cat notes.md` classify identically. Nothing on this path
/// (or upstream's own `ShellTool::run_with_security_in_context`, which never
/// calls the vendored `validate_command_execution`/`is_command_allowed`
/// allowlist) confines a `Read`-class command to the workspace at execution
/// time; the only backstop the vendored runtime ships,
/// `scan_command_for_cross_profile`, exists for a different boundary
/// (sibling-profile isolation) and says so itself: "airtight process
/// confinement … is deliberate follow-up work, not provided here." So a bare
/// classifier-`Read` command is not yet a safe zero-approval command in this
/// codebase's threat model, where "the agent's own workspace" is the entire
/// premise the free pass rests on (see the issue's own framing). Every read
/// this downgrades is additionally checked by
/// [`shell_command_reaches_outside_cwd`] and gated back to `Consequence` if
/// any argument names an absolute path, a `~` home-dir reference, or a `..`
/// traversal segment — the three ways a token can point outside the working
/// directory without resolving anything against the real workspace root.
fn shell_consequence(args: &serde_json::Value) -> Consequence {
    let gated = Consequence {
        group: EffectGroup::Other,
        reach: Reach::Consequence,
        standing: Standing::PerCall,
    };
    let Some(command) = args.get(SHELL_COMMAND_KEY).and_then(|v| v.as_str()) else {
        // The tool's own schema requires it, so this is a call that could not
        // have run. Gate it rather than guess.
        return gated;
    };
    let declared = args.get(SHELL_CATEGORY_KEY).and_then(|v| v.as_str());
    if shell_command_is_read(command, declared) && !shell_command_reaches_outside_cwd(command) {
        // A read of the agent's own workspace changes nothing, reaches nobody
        // and is billed for nothing — the shape `glob` and `grep` (the tools)
        // have carried since #462.
        return Consequence {
            group: EffectGroup::Other,
            reach: Reach::Nothing,
            standing: Standing::PerCall,
        };
    }
    gated
}

/// Classify a `git_operations` call from its `operation` argument (issue #877).
///
/// # ⚠️ The exposure this downgrade accepts
///
/// **Read this before widening [`GIT_READ_ONLY_OPERATIONS`].** `git_operations`
/// runs against the agent's own workspace — `GitOperationsTool::new(security,
/// workspace)` in [`crate::harness::toolbelt::code_tools`] — through the
/// vendored `run_git_command_in`, which is a bare
/// `Command::new("git").args(args).current_dir(cwd)` with **no
/// `GIT_CONFIG_NOSYSTEM`, no `-c` overrides and no environment scrub**. Several
/// git config keys name a command to run (`core.fsmonitor`, `core.pager`,
/// `diff.external`, `core.sshCommand`), and the repository config lives in a
/// directory `file_write` can write to — so a `.git/config` the agent authored
/// can decide what executes when any of these operations runs.
///
/// That is the identical primitive `read_workspace_state` is gated for, and its
/// note in [`DECLARED`] says to revert that stopgap "once a hardened `run_git`
/// is vendored, **and not before**", tracking the work at
/// `tinyhumansai/openhuman#5494`. Downgrading here accepts that exposure for
/// these six operations ahead of that hardening; it is a deliberate scope
/// decision recorded on issue #877, not an oversight. When #5494 lands, the two
/// tools should be reconciled — either both downgraded or both gated — because
/// today they run the same command against the same directory.
///
/// # Fail-closed, by the [`shell_consequence`] template
///
/// Five mechanisms, all of which must hold for a call to be downgraded:
///
/// 1. The gated verdict is built first and every early return uses it.
/// 2. A missing or non-string `operation` gates — the tool's own schema
///    requires it, so such a call could not have run.
/// 3. Only **affirmative** membership of [`GIT_READ_ONLY_OPERATIONS`]
///    downgrades. `push`, `pull`, `fetch`, `merge`, `rebase` and `clone` are in
///    neither upstream list, so they are unclassified — and unclassified gates,
///    by construction rather than by a rule someone has to remember.
/// 4. Comparison is exact and case-sensitive, matching upstream's `matches!`.
/// 5. There is no self-declared hint to honour here, so there is nothing that
///    could lower the verdict — the escalate-only rule `shell` needs is
///    satisfied vacuously.
///
/// # Why a local list rather than the vendored hook
///
/// `Tool::external_effect_with_args` is the only public route into upstream's
/// judgement, and it is **tier-coupled**:
///
/// ```text
/// self.requires_write_access(operation)
///     && self.security.gate_decision(CommandClass::Write) == GateDecision::Prompt
/// ```
///
/// Calling it here would import OpenHuman's desktop tier into a gate that
/// answers the tier question one layer up — and under a policy whose
/// `gate_decision` is not `Prompt` it returns `false` for a genuine **write**,
/// i.e. it fails **open**. `requires_write_access` and `is_read_only` are
/// private inherent methods, so there is no untainted route to borrow.
///
/// A second list that can drift is exactly what issue #877 warns against, so
/// the vendored hook is used as a **test oracle** instead —
/// `the_read_only_set_matches_the_vendored_classifier` drives it at
/// `AutonomyLevel::Supervised`, where `gate_decision(Write)` *is* `Prompt` and
/// the conjunction reduces to the operation test alone. A list that cannot
/// drift silently is not the failure mode being warned about.
fn git_operations_consequence(args: &serde_json::Value) -> Consequence {
    let gated = Consequence {
        group: EffectGroup::Other,
        reach: Reach::Consequence,
        standing: Standing::PerCall,
    };
    let Some(operation) = args.get(GIT_OPERATION_KEY).and_then(|v| v.as_str()) else {
        // The tool's own schema marks `operation` required, so this is a call
        // that could not have run. Gate it rather than guess.
        return gated;
    };
    if GIT_READ_ONLY_OPERATIONS.contains(&operation) {
        return Consequence {
            group: EffectGroup::Other,
            reach: Reach::Nothing,
            standing: Standing::PerCall,
        };
    }
    gated
}

/// The `git_operations` subcommands that only read the repository.
///
/// Mirrors the vendored `GitOperationsTool::is_read_only` set exactly. It is a
/// copy, and the copy is load-bearing — see the "why a local list" note on
/// [`git_operations_consequence`] for why the vendored hook cannot be called
/// from here, and `the_read_only_set_matches_the_vendored_classifier` for the
/// oracle that fails if upstream ever reclassifies one of these.
///
/// Everything absent from this list gates, including the operations upstream
/// itself does not classify (`push`, `pull`, `fetch`, `merge`, `rebase`,
/// `clone`).
const GIT_READ_ONLY_OPERATIONS: &[&str] = &["status", "diff", "log", "show", "branch", "rev-parse"];

/// The consequence of one MCP bridge call — the fail-closed base for both
/// [`MCP_CALL_TOOL`] and [`MCP_REGISTRY_TOOL_CALL`] (#1124).
///
/// One name carries every remote tool on every server, so this cannot be
/// classified from the name: a call that reads a Jira ticket and one that files
/// one arrive here identically. The distinguishing information is the (server,
/// tool) pair in the arguments — but *whether that pair only reads* is an
/// **operator declaration** per server, which is company context. A pure
/// classifier cannot reach it, so this answers the cautious base every call
/// keeps until proven otherwise: a call through a third-party server can perform
/// any effect that server advertises, so it parks under `supervised` and `auto`
/// and holds no standing grant.
///
/// The downgrade — the whole point of the issue — is applied by
/// [`mcp_call_reach`], which the policy calls **with** the declaration. That the
/// base is `Reach::Consequence` is what makes the split fail closed by
/// construction: an undeclared server, a server whose declaration does not name
/// this tool, a missing or non-string `server`/`tool` argument, and a build
/// whose policy carries no declaration all resolve here, never to a downgrade.
fn mcp_call_tool_consequence(_args: &serde_json::Value) -> Consequence {
    Consequence {
        group: EffectGroup::Other,
        reach: Reach::Consequence,
        standing: Standing::PerCall,
    }
}

/// The pure, fail-closed half of workspace authorship grading (issue #877).
///
/// A call only becomes safe once the live company tree confirms that the node
/// was both created and last written by the calling agent. That lookup belongs
/// to `ApprovalPolicy`, alongside the MCP declaration lookup; this function is
/// what callers without that company context receive.
fn workspace_mutation_consequence(_args: &serde_json::Value) -> Consequence {
    Consequence {
        group: EffectGroup::Other,
        reach: Reach::Consequence,
        standing: Standing::PerCall,
    }
}

/// The operator's declaration of which remote MCP tools **only read**, keyed by
/// the `(server, tool)` pair the bridge call carries (#1124).
///
/// This is the third per-server list beside `allowed_tools` / `disallowed_tools`
/// (`McpServerDecl::read_only_tools`), flattened to a set the gate can consult
/// in one lookup. It arrives on the policy — [`ApprovalPolicy::with_mcp_reads`]
/// — because [`consequence_of`] is pure and company-blind; the policy is the one
/// layer that has both the live call and this declaration.
///
/// The key is the server **as the call names it**: the `server` display name for
/// [`MCP_CALL_TOOL`], the `server_id` for [`MCP_REGISTRY_TOOL_CALL`]. Two
/// registries, one set — a caller populates it from whichever declaration source
/// keys each server the way its bridge call will. Nothing here interprets the
/// server key; membership is exact, so a server the operator has not declared a
/// read for is simply absent, which is the gated answer.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct McpReadSet {
    pairs: std::collections::HashSet<(String, String)>,
}

impl McpReadSet {
    /// Builds the set from `(server, tool)` pairs. Empty — the default — means
    /// no remote tool is declared read-only, so every bridge call gates, which
    /// is exactly what every construction site that sets no declaration wants.
    pub fn from_pairs(pairs: impl IntoIterator<Item = (String, String)>) -> Self {
        Self {
            pairs: pairs.into_iter().collect(),
        }
    }

    /// Has the operator declared this exact remote tool on this server a read?
    pub fn contains(&self, server: &str, tool: &str) -> bool {
        self.pairs.contains(&(server.to_string(), tool.to_string()))
    }

    /// Whether any read is declared at all, so a caller can skip the lookup for
    /// a policy that carries no declaration.
    pub fn is_empty(&self) -> bool {
        self.pairs.is_empty()
    }
}

/// The `(server, tool)` pair an MCP bridge call names, read from the arguments
/// with the keys the tool's own schema requires (#1124).
///
/// `None` when the tool is neither bridge tool, or when either key is absent or
/// not a string — the tools' schemas mark both required, so such a call could
/// not have run, and guessing a pair for it would be inventing a verdict for a
/// call that never happened. The keys differ by tool: `mcp_call_tool` names its
/// server `server` and its remote tool `tool`; `mcp_registry_tool_call` names
/// them `server_id` and `tool_name`.
fn mcp_call_pair<'a>(tool: &str, args: &'a serde_json::Value) -> Option<(&'a str, &'a str)> {
    let (server_key, tool_key) = if tool.eq_ignore_ascii_case(MCP_CALL_TOOL) {
        (MCP_CALL_SERVER_KEY, MCP_CALL_TOOL_KEY)
    } else if tool.eq_ignore_ascii_case(MCP_REGISTRY_TOOL_CALL) {
        (MCP_REGISTRY_SERVER_KEY, MCP_REGISTRY_TOOL_KEY)
    } else {
        return None;
    };
    let object = args.as_object()?;
    let server = object.get(server_key)?.as_str()?;
    let remote_tool = object.get(tool_key)?.as_str()?;
    Some((server, remote_tool))
}

/// The reach of one MCP bridge call, downgraded to [`Reach::ExternalRead`]
/// **only** when the operator has declared this call's remote tool a read on
/// this server (#1124).
///
/// The one place the (server, tool) pair meets the declaration. Every other
/// answer is [`Reach::Consequence`], the [`mcp_call_tool_consequence`] base:
///
///  1. a tool that is neither bridge tool — `None` from [`mcp_call_pair`];
///  2. a call whose `server` / `tool` argument cannot be read — same;
///  3. a server the operator has not declared this tool a read on — the set
///     lookup misses.
///
/// So the downgrade is affirmative-membership-only, and the gate stays fail
/// closed by construction rather than by a rule someone has to remember — the
/// same shape [`git_operations_consequence`] takes against its read set. Returns
/// the whole [`Consequence`] rather than a bare [`Reach`] so the policy replaces
/// the base verdict wholesale, exactly as the argument graders do.
///
/// The downgrade is [`Reach::ExternalRead`], not [`Reach::Nothing`], and this is
/// the Composio-read precedent (#559) rather than a fresh choice: a remote MCP
/// read reaches a *third party's* server with the company's own connected
/// credential. It changes nothing there or here and is billed for nothing, so
/// `supervised` and `auto` let it run — the whole point of the issue — but a
/// `readonly` desk still denies it, because that tier's contract is that nothing
/// outside the company is reached at all. Folding it into [`Reach::Nothing`]
/// would break that contract; folding it into [`Reach::Money`] would bill the
/// operator for every page read.
///
/// [`Standing::PerCall`] stays: the declaration says "this reads", which is not
/// "hand every remote tool on this server over for a week". A read-only remote
/// tool nevertheless never parks under `auto` on its own, so nothing is grantable
/// here for the tier to consult.
pub fn mcp_call_reach(tool: &str, args: &serde_json::Value, reads: &McpReadSet) -> Consequence {
    let base = mcp_call_tool_consequence(args);
    if reads.is_empty() {
        return base;
    }
    match mcp_call_pair(tool, args) {
        Some((server, remote_tool)) if reads.contains(server, remote_tool) => Consequence {
            group: EffectGroup::Other,
            reach: Reach::ExternalRead,
            standing: Standing::PerCall,
        },
        _ => base,
    }
}

/// Whether `tool` is one of the MCP bridge tools whose reach a read declaration
/// can downgrade (#1124). The policy asks this before consulting its
/// declaration, so a non-bridge tool takes the plain [`consequence_of`] path.
pub fn is_mcp_bridge_tool(tool: &str) -> bool {
    tool.eq_ignore_ascii_case(MCP_CALL_TOOL) || tool.eq_ignore_ascii_case(MCP_REGISTRY_TOOL_CALL)
}

/// Lexical backstop for [`shell_consequence`]: does any whitespace-separated
/// token in `command` name a location outside the agent's working directory?
///
/// This is deliberately a text scan, not a path resolver — it needs no
/// `action_dir`/cwd context, so it stays a pure function of the command
/// string like the rest of this module's classifiers. It catches the
/// realistic, non-adversarial escape vectors a reviewer would actually type —
/// an absolute path (`/etc/passwd`), a home-dir reference (`~/.ssh/id_rsa`),
/// a `--flag=/absolute/value`, or a `..` traversal segment — without claiming
/// to be airtight: a symlink inside the workspace pointing outside it is
/// invisible to a lexical scan, and so would be a `cd` out of the workspace
/// followed by a relative read, except that `cd` is not itself in the
/// vendored classifier's `READ_ONLY_BASES`, so any segment naming it already
/// fails the *whole* command closed to `Write` before this function is ever
/// consulted (`classify_command` takes the max across `;`/`&&`/`||`-separated
/// segments). Full process-level confinement remains upstream follow-up work,
/// same as it is for the cross-profile guard this mirrors.
fn shell_command_reaches_outside_cwd(command: &str) -> bool {
    command.split_whitespace().any(|word| {
        // A `--flag=/value` or `--flag=~/value` carries the path after `=`.
        let candidate = word.rsplit('=').next().unwrap_or(word);
        let candidate = candidate.trim_matches(|c| c == '"' || c == '\'');
        candidate.starts_with('/')
            || candidate.starts_with('~')
            || candidate.split('/').any(|segment| segment == "..")
    })
}

/// Is this command provably read-only, according to the vendored runtime's own
/// classifier? A self-declared `category` may only escalate.
#[cfg(feature = "openhuman")]
fn shell_command_is_read(command: &str, declared: Option<&str>) -> bool {
    use openhuman_core::security::{CommandClass, SecurityPolicy};

    // `classify_command` is a pure function of the command text — it reads no
    // field of the policy it hangs off — so the default instance is the whole
    // configuration this needs. The tier question is answered above this layer,
    // by the `Reach` this returns.
    let policy = SecurityPolicy::default();
    let mut class = policy.classify_command(command);
    if class == CommandClass::Write && shell_argv_read_exception(command) {
        class = CommandClass::Read;
    }
    if let Some(declared) = declared.and_then(SecurityPolicy::parse_declared_class) {
        class = class.max(declared);
    }
    matches!(class, CommandClass::Read)
}

/// Read-only argv forms whose base is deliberately absent from the vendored
/// name-only allowlist (issue #972).
///
/// This is a fallback for `Write`, never a replacement for the vendor's
/// network/destructive grading. It accepts one parsed simple command only:
/// shell operators, expansions, comments, malformed quoting and every unknown
/// base fail closed before any flag-specific rule is considered.
#[cfg(feature = "openhuman")]
fn shell_argv_read_exception(command: &str) -> bool {
    let Some(argv) = parse_simple_shell_argv(command) else {
        return false;
    };
    let Some((base, args)) = argv.split_first() else {
        return false;
    };

    match base.as_str() {
        "sed" => {
            !shell_argv_has_option(args, 'i', "--in-place")
                // A file-supplied program is opaque to this classifier and may
                // contain sed's `w` or `e` commands.
                && !shell_argv_has_option(args, 'f', "--file")
        }
        "sort" => {
            !shell_argv_has_option(args, 'o', "--output")
                // GNU sort may execute this program while spilling runs.
                && !shell_argv_has_long_option(args, "--compress-program")
        }
        "awk" => {
            !shell_argv_has_option(args, 'f', "--file")
                && !shell_argv_has_option(args, 'E', "--exec")
                && args.iter().all(|arg| {
                    let compact: String = arg.chars().filter(|c| !c.is_whitespace()).collect();
                    !arg.contains('>')
                        && !arg.contains('|')
                        && !compact.contains("system(")
                        && !arg.contains("@load")
                })
        }
        _ => false,
    }
}

/// Parse exactly one shell simple-command argv.
///
/// This intentionally supports less syntax than a shell. Quoting and escaped
/// characters are enough for ordinary sed/awk programs; syntax that could add
/// commands or derive flags at execution time is ambiguous here and rejected.
#[cfg(feature = "openhuman")]
fn parse_simple_shell_argv(command: &str) -> Option<Vec<String>> {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Quote {
        None,
        Single,
        Double,
    }

    // Command substitution is refused even when quoting makes it literal. The
    // existing shell execution guard uses the same deliberately lexical rule.
    if command.contains("$(") || command.contains('`') || command.contains('\0') {
        return None;
    }

    let mut argv = Vec::new();
    let mut word = String::new();
    let mut word_started = false;
    let mut quote = Quote::None;
    let mut chars = command.chars();

    while let Some(ch) = chars.next() {
        match quote {
            Quote::None => match ch {
                '\'' => {
                    quote = Quote::Single;
                    word_started = true;
                }
                '"' => {
                    quote = Quote::Double;
                    word_started = true;
                }
                '\\' => {
                    let escaped = chars.next()?;
                    if escaped == '\n' || escaped == '\r' {
                        return None;
                    }
                    word.push(escaped);
                    word_started = true;
                }
                c if c.is_whitespace() => {
                    if word_started {
                        argv.push(std::mem::take(&mut word));
                        word_started = false;
                    }
                }
                // These either join commands, redirect I/O, start a subshell,
                // or make argv depend on runtime expansion/comment parsing.
                ';' | '|' | '&' | '<' | '>' | '(' | ')' | '$' | '#' => return None,
                _ => {
                    word.push(ch);
                    word_started = true;
                }
            },
            Quote::Single => {
                if ch == '\'' {
                    quote = Quote::None;
                } else {
                    word.push(ch);
                }
            }
            Quote::Double => match ch {
                '"' => quote = Quote::None,
                '\\' => {
                    let escaped = chars.next()?;
                    if escaped == '\n' || escaped == '\r' {
                        return None;
                    }
                    word.push(escaped);
                }
                // Double-quoted parameters still make the final argv unknown.
                '$' => return None,
                _ => word.push(ch),
            },
        }
    }

    if quote != Quote::None {
        return None;
    }
    if word_started {
        argv.push(word);
    }
    (!argv.is_empty()).then_some(argv)
}

#[cfg(feature = "openhuman")]
fn shell_argv_has_option(args: &[String], short: char, long: &str) -> bool {
    let mut options = true;
    for arg in args {
        if options && arg == "--" {
            options = false;
            continue;
        }
        if !options {
            continue;
        }
        if let Some(name) = arg
            .strip_prefix("--")
            .map(|_| arg.split_once('=').map_or(arg.as_str(), |(name, _)| name))
        {
            let name = name.strip_prefix("--").unwrap_or(name);
            let long = long.strip_prefix("--").unwrap_or(long);
            if !name.is_empty() && long.starts_with(name) {
                return true;
            }
            continue;
        }
        if arg
            .strip_prefix('-')
            .is_some_and(|flags| !flags.is_empty() && flags.contains(short))
        {
            return true;
        }
    }
    false
}

#[cfg(feature = "openhuman")]
fn shell_argv_has_long_option(args: &[String], long: &str) -> bool {
    shell_argv_has_option(args, '\0', long)
}

/// Without the harness feature the classifier is not linked in, so nothing here
/// can tell a read from an act — and the cautious answer is that it is an act.
#[cfg(not(feature = "openhuman"))]
fn shell_command_is_read(_command: &str, _declared: Option<&str>) -> bool {
    false
}

/// What scope a standing permission for this call may be minted with — or that
/// it may not be minted at all (issue #2148).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StandingMintScope {
    /// Mint with no scope. The tool's name is the whole of what it can do, so
    /// there is nothing to narrow.
    Unscoped,
    /// Mint confined to this slice of the tool — a host, or a Composio toolkit.
    Scoped(String),
    /// Do not mint. The sentence says why, and is written to be read by an
    /// operator rather than by a developer.
    Refused(String),
}

/// Whether a standing permission for this call may be minted, and how narrow.
///
/// ## The invariant this exists to enforce
///
/// [`StandingGrant::admits_scope`](crate::runtime::grants::StandingGrant::admits_scope)
/// treats an **unscoped** grant as admitting everything — correct for a journal
/// line written before the field existed, catastrophic for a permission that
/// was supposed to name one host or one toolkit. So a tool whose declaration is
/// [`Standing::ScopedGrantable`] must never reach the journal without a scope.
///
/// Today it cannot: the only classification that answers `ScopedGrantable` is
/// [`web_fetch_consequence`], which answers it exclusively in the arm where the
/// scope was already read, and degrades to [`Standing::PerCall`] otherwise —
/// "tying both answers to one read makes that unrepresentable", as it says.
///
/// That is a property of one call site, and the next `ScopedGrantable` entry
/// that derives its scope by a separate route would break it in silence. This
/// makes the requirement explicit at the mint, where the consequence of getting
/// it wrong actually lands, and `no_scope_required_tool_can_mint_unscoped`
/// walks the table so a future violator fails a test rather than an operator.
///
/// ## Why a denial may go unscoped and an approval may not
///
/// The asymmetry is the whole point of the scope. An unscoped **approval**
/// admits every host; an unscoped **denial** refuses every host, which is
/// broader than asked but fails in the safe direction — and refusing to mint it
/// would leave an operator unable to decline a tool for a period at all. A
/// standing denial is a real state the console offers (issue #1458), so it
/// keeps working.
pub fn standing_mint_scope(
    tool: &str,
    args: &serde_json::Value,
    verdict: crate::ports::types::Verdict,
) -> StandingMintScope {
    decide_standing_mint_scope(
        consequence_of(tool, args).standing,
        standing_scope_of(tool, args),
        verdict,
    )
}

/// [`standing_mint_scope`] over an explicit declaration.
///
/// Split out so the combination the shipped table cannot currently produce —
/// scope-required, no scope, approving — is still reachable from a test. A rule
/// whose failing case can only be described in prose is a rule nobody has run.
fn decide_standing_mint_scope(
    standing: Standing,
    scope: Option<String>,
    verdict: crate::ports::types::Verdict,
) -> StandingMintScope {
    match (scope, standing, verdict) {
        (Some(scope), _, _) => StandingMintScope::Scoped(scope),
        (None, Standing::ScopedGrantable, crate::ports::types::Verdict::Approve) => {
            StandingMintScope::Refused(
                "this permission has to name the account or address it covers, and this call \
                 does not say which one — approve it once instead"
                    .to_string(),
            )
        }
        (None, _, _) => StandingMintScope::Unscoped,
    }
}

pub fn standing_scope_of(tool: &str, args: &serde_json::Value) -> Option<String> {
    // The mint side and the live call must read the host with identical code, or
    // a grant could be minted that never matches its own tool.
    if [WEB_FETCH, "http_request"]
        .iter()
        .any(|candidate| tool.eq_ignore_ascii_case(candidate))
    {
        return web_fetch_scope_of(args);
    }
    if !tool.eq_ignore_ascii_case(COMPOSIO_EXECUTE) {
        return None;
    }
    // Same reader as the classifier, so a call it could not read a slug out of
    // cannot resolve a toolkit here either — a scoped grant refuses to admit
    // `None`, which is the safe direction.
    let slug = composio_action_slug(args).ok()?;
    composio_toolkit_of(slug)
}

/// The catalogued toolkit an action slug belongs to, or `None` when the
/// catalogue has never heard of it.
#[cfg(feature = "openhuman")]
fn composio_toolkit_of(slug: &str) -> Option<String> {
    // The curated catalogue moved upstream into TinyMemory's Composio
    // vocabulary; `tinymemory_api` re-exports `tinymemory_bus::composio`
    // wholesale, so this is the same items openhuman itself now uses.
    use tinymemory_api::composio::catalogs::catalog_for_toolkit;
    use tinymemory_api::composio::scopes::toolkit_from_slug;
    let toolkit = toolkit_from_slug(slug)?;
    // The slug's prefix is *some* word for every non-empty slug, so the
    // catalogue lookup is what separates a real toolkit from a typo.
    catalog_for_toolkit(&toolkit).is_some().then_some(toolkit)
}

/// Without the harness feature the curated catalogue is not linked in — the same
/// seam `composio_catalog_lookup` straddles, answered the same cautious way.
#[cfg(not(feature = "openhuman"))]
fn composio_toolkit_of(_slug: &str) -> Option<String> {
    None
}

/// Is this Composio action slug a read, according to the provider's own
/// curated catalogue? Unknown is **not** a read.
#[cfg(feature = "openhuman")]
fn composio_catalog_lookup(slug: &str) -> CatalogLookup {
    use tinymemory_api::composio::catalogs::catalog_for_toolkit;
    use tinymemory_api::composio::scopes::{ToolScope, find_curated, toolkit_from_slug};
    let Some(toolkit) = toolkit_from_slug(slug) else {
        return CatalogLookup::UnknownToolkit { toolkit: None };
    };
    let Some(catalog) = catalog_for_toolkit(&toolkit) else {
        return CatalogLookup::UnknownToolkit {
            toolkit: Some(toolkit),
        };
    };
    match find_curated(catalog, slug).map(|entry| entry.scope) {
        Some(ToolScope::Read) => CatalogLookup::Curated { read: true },
        Some(_) => CatalogLookup::Curated { read: false },
        None => CatalogLookup::UncuratedAction { toolkit },
    }
}

/// Without the harness feature the curated catalogue is not linked in, and no
/// `composio_execute` call can be made either — only replayed from a journal
/// line an openhuman build wrote. Cautious is the only honest answer.
///
/// [`CatalogueAbsent`](CatalogLookup::CatalogueAbsent) rather than
/// `UnknownToolkit { toolkit: None }` since issue #1818: the caller says so in
/// the log and skips the verb fallback here, because a build that cannot place
/// `GITHUB_LIST_PULL_REQUESTS` either has not earned the right to infer
/// anything from `GITHUB_LIST_SOMETHING_ELSE`.
#[cfg(not(feature = "openhuman"))]
fn composio_catalog_lookup(_slug: &str) -> CatalogLookup {
    CatalogLookup::CatalogueAbsent
}

/// Say once, loudly, that this binary gates every Composio action as a send
/// because it links no catalogue (issue #1818).
///
/// Per-call the fact is already in the log line beside the slug; this is the
/// one an operator greps for when a whole desk has stalled. `Once`, because the
/// answer is a property of the build and repeating it per call would bury the
/// slug lines it is meant to explain.
fn catalogue_absent_warning() {
    static SAID: std::sync::Once = std::sync::Once::new();
    SAID.call_once(|| {
        tracing::warn!(
            catalogue_absent = true,
            "[policy] this build links no curated Composio action catalogue (the `openhuman` \
             feature is off), so EVERY `{COMPOSIO_EXECUTE}` call classifies as a send and parks \
             — including reads. That is an over-gate, not a verdict about any action (issue \
             #1818)."
        );
    });
}

/// Does this action slug's own verb say it only reads (issue #1818)?
///
/// The fallback for a slug the curated catalogue cannot place. Composio action
/// slugs are `TOOLKIT_VERB_OBJECT` by convention — `GITHUB_LIST_REPOSITORY_ISSUES`,
/// `GMAIL_SEND_EMAIL` — and that verb is the most reliable thing about a name
/// nobody has classified.
///
/// # The rule
///
/// A read needs a read verb present and **no** mutating verb anywhere. Segments
/// are matched whole, which is what keeps `GITHUB_LIST_STARGAZERS` a read —
/// `STARGAZERS` is not `STAR` — and `GMAIL_LIST_DRAFTS` a read, since `DRAFTS`
/// is not `DRAFT`. A slug with no verb in either list, `GITHUB_INVENT_A_NEW_VERB`,
/// is **not** a read: this asks for evidence and takes its absence as a send,
/// which is the whole difference from upstream's `classify_unknown`.
///
/// # Why a mutating verb vetoes wherever it appears
///
/// An earlier draft let the first verb decide, so `..._GET_DRAFT` could keep the
/// read verdict with `DRAFT` read as the noun it is. That is the nicer answer
/// for that slug and the wrong rule: it also hands the read verdict to
/// `..._GET_AND_UPDATE_...` and `..._FIND_OR_CREATE_...`, which mutate. Every
/// narrower exemption tried — "a mutating word is allowed in the object slot
/// immediately after the read verb" — was defeated by a real catalogue entry:
/// `GOOGLESHEETS_FIND_REPLACE` is a curated **write** with exactly that shape,
/// and it is a find-and-replace with the conjunction elided. A noun and an
/// elided second verb are not distinguishable from the name, so the rule takes
/// the side that over-gates.
///
/// The price is paid by slugs like `GMAIL_GET_DRAFT` — and it is not really
/// paid at all: that one is *curated*, so it is answered by the catalogue and
/// never reaches this function. What arrives here is only what nobody has
/// classified.
///
/// # Why over-gating is the safe direction
///
/// Every verdict this returns `false` for is the behaviour before #1818 — a
/// park an operator can approve. Every verdict it returns `true` for runs
/// unattended. So the lists are asymmetric on purpose: the read verbs are few
/// and unambiguous, the mutating list is generous, and anything unrecognised
/// falls to the cautious side.
///
/// [`the_fallback_never_calls_a_curated_write_a_read`] is what holds the
/// vocabulary honest: it runs this over all ~680 hand-classified actions in the
/// vendored catalogue and fails if a single `Write` or `Admin` slips through.
/// `ANSWER` and `REPLACE` are in the list below because that test found them.
///
/// [`the_fallback_never_calls_a_curated_write_a_read`]: tests::the_fallback_never_calls_a_curated_write_a_read
fn composio_slug_reads_by_verb(slug: &str) -> bool {
    /// Verbs that positively say an action only reads.
    const READS: &[&str] = &[
        "COUNT", "DESCRIBE", "FETCH", "FIND", "GET", "LIST", "LOOKUP", "QUERY", "READ", "RETRIEVE",
        "VIEW", "SEARCH",
    ];
    /// Verbs that say it mutates, spends, destroys, or reaches a counterparty.
    /// Generous on purpose: a wrong entry here only over-gates, and a missing
    /// one runs a write unattended.
    const MUTATES: &[&str] = &[
        "ACCEPT",
        "ACTIVATE",
        "ADD",
        "ANSWER",
        "APPEND",
        "APPROVE",
        "ARCHIVE",
        "ASSIGN",
        "CANCEL",
        "CHARGE",
        "CLEAR",
        "CLOSE",
        "COMPLETE",
        "CONFIRM",
        "COPY",
        "CREATE",
        "DEACTIVATE",
        "DECLINE",
        "DELETE",
        "DEPLOY",
        "DESTROY",
        "DISABLE",
        "DISMISS",
        "DISPATCH",
        "DRAFT",
        "DUPLICATE",
        "EDIT",
        "ENABLE",
        "EXECUTE",
        "FOLLOW",
        "FORK",
        "GRANT",
        "IMPORT",
        "INSERT",
        "INVITE",
        "JOIN",
        "KICK",
        "LEAVE",
        "LOCK",
        "MARK",
        "MERGE",
        "MODIFY",
        "MOVE",
        "MUTE",
        "PATCH",
        "PAY",
        "PIN",
        "POST",
        "PUBLISH",
        "PURGE",
        "PUT",
        "REACT",
        "REFUND",
        "REJECT",
        "REMOVE",
        "RENAME",
        "REOPEN",
        "REPLACE",
        "REPLY",
        "RESET",
        "RESTORE",
        "REVOKE",
        "RUN",
        "SEND",
        "SET",
        "SHARE",
        "STAR",
        "START",
        "STOP",
        "SUBMIT",
        "SUBSCRIBE",
        "TRANSFER",
        "TRASH",
        "TRIGGER",
        "UNARCHIVE",
        "UNFOLLOW",
        "UNLOCK",
        "UNPIN",
        "UNSHARE",
        "UNSTAR",
        "UNSUBSCRIBE",
        "UPDATE",
        "UPLOAD",
        "UPSERT",
        "WIPE",
        "WRITE",
    ];

    let upper = slug.trim().to_ascii_uppercase();
    let mut reads = false;
    for segment in upper.split('_').filter(|segment| !segment.is_empty()) {
        if MUTATES.contains(&segment) {
            return false;
        }
        reads |= READS.contains(&segment);
    }
    reads
}

/// A tool with no declaration.
///
/// **Never grantable** — that is the whole of issue #444's second half. `Other`
/// used to be the bucket a tool fell into by omission *and* the bucket that
/// conferred a week-long capability, so adding a tool and forgetting to think
/// about it handed it the longest permission available.
///
/// The name heuristics survive here, and only here, for [`Reach`]. Dropping
/// them would park every read in a build configuration whose tools nobody
/// remembered to declare — trading a silent over-grant for a silent
/// over-prompt. The coverage test is what stops a *registered* tool reaching
/// this path at all.
fn undeclared(name: &str) -> Consequence {
    // `describe` is deliberately absent. This fallback is a courtesy for an
    // unregistered read, not a second classifier to trust with an unreviewed
    // capability: adding it would let an undeclared tool claim it only reads.
    // Declare a known `describe_*` tool instead, as `describe_skill` does for
    // issue #845, so its reach is an explicit policy decision.
    const READ_ONLY_PREFIXES: &[&str] = &[
        "read",
        "list",
        "get",
        "search",
        "recall",
        "query",
        "peek",
        "inspect",
        "view",
        "memory_recall",
        "memory_search",
    ];
    let reads = READ_ONLY_PREFIXES.iter().any(|p| name.starts_with(p));
    Consequence {
        group: undeclared_group(name),
        reach: if reads {
            Reach::Nothing
        } else {
            Reach::Consequence
        },
        standing: Standing::PerCall,
    }
}

/// The consequence-word heuristics, kept for undeclared tools so an approval
/// card for one is still labelled as well as it was before.
fn undeclared_group(name: &str) -> EffectGroup {
    if name.contains("pay") || name.contains("transfer") || name.starts_with("spend") {
        EffectGroup::Spend
    } else if name.contains("email") || name.contains("send") || name.contains("message") {
        EffectGroup::Send
    } else if name.contains("sign") || name.contains("file") || name.contains("filing") {
        EffectGroup::Sign
    } else if name.contains("publish") || name.contains("post") || name.contains("deploy") {
        EffectGroup::Publish
    } else if name.contains("hire") || name.contains("contract") {
        EffectGroup::Hire
    } else if name.contains("identity") || name.contains("handle") {
        EffectGroup::Identity
    } else {
        EffectGroup::Other
    }
}

#[cfg(test)]
#[path = "consequence_composio_tests.rs"]
mod consequence_composio_tests;
#[cfg(test)]
#[path = "consequence_fetch_grant_tests.rs"]
mod consequence_fetch_grant_tests;
#[cfg(test)]
#[path = "consequence_hosting_tests.rs"]
mod consequence_hosting_tests;
#[cfg(test)]
#[path = "consequence_mcp_roster_tests.rs"]
mod consequence_mcp_roster_tests;
#[cfg(test)]
#[path = "consequence_scope_labels_tests.rs"]
mod consequence_scope_labels_tests;
#[cfg(test)]
#[path = "consequence_shell_git_mcp_tests.rs"]
mod consequence_shell_git_mcp_tests;
