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
/// deadline, and it still parks under `auto`. That variant exists because an
/// outward fetch needs the delegation half and must not have the unattended
/// half — see its own documentation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Standing {
    /// An operator may grant this to a teammate until a deadline — **and**,
    /// since issue #560, may run unattended under the `auto` tier. See the note
    /// on [`Standing`] before loosening a tool to this.
    Grantable,
    /// An operator may grant this to a teammate until a deadline, but it still
    /// parks under `auto` (issue #673).
    ///
    /// The variant exists because the two questions [`Standing`] answers pull
    /// apart for an outward fetch. "May maya fetch `https://docs.rs` for the
    /// next few days?" is a sentence an operator can consent to. "May every
    /// agent fetch any address, unattended, for as long as the company sits in
    /// `auto`?" is not — and marking the tool [`Grantable`](Self::Grantable) to
    /// obtain the first would have silently bought the second, because
    /// [`Consequence::parks_under_auto`] reads exactly that field.
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

/// The outward-fetch tool a standing grant may be scoped to a host on (#673).
///
/// Only this one of the three web tools. `http_request` and `curl` can mutate,
/// so a host-scoped grant on them would consent to *writing* to that host —
/// a different act from the read this issue is about, and one nobody asked for.
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
    // The three shapes issue #444 names, plus the two web tools that share
    // `http_request`'s shape. A standing grant on any of these is a standing
    // grant on "anything the sandbox permits", which is not a sentence an
    // operator can consent to.
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
    // The agent persona *instructs* every agent to call `mcp_list_servers` (and
    // `mcp_list_tools` for a specific server) rather than answer a capability
    // question from memory, so parking them made the guidance that exists to
    // prevent stale answers cost an operator approval to follow (issue #443).
    //
    // `mcp_list_servers` and `mcp_registry_list_tools` read process-local
    // registration state, credentials already redacted. `mcp_list_tools` is
    // NOT local — it is a `tools/list` round trip to the operator-configured
    // server. It is `Nothing` all the same: it changes nothing there or here
    // and is billed for nothing, and a desk that cannot ask a server what it
    // offers cannot use one. Whether a call *reaches* is the question this
    // module stopped asking; what it costs is the question it asks instead.
    //
    // Calling *through* a server is a consequence and stays per-call: it can
    // perform any effect the third-party server advertises.
    d("mcp_list_servers", EffectGroup::Other, Reach::Nothing),
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
    // The nine `hosting_*` tools openhuman ships in `src/openhuman/hosting/`.
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
    // Five reads, per openhuman's own `hosting/README.md`, which labels each of
    // them "Read-only.". They ask the provider what exists and what it did;
    // nothing leaves this company and nothing is spent.
    d(
        "hosting_deployment_status",
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
/// classifiers were four hand-written `if` arms in [`consequence_of`] and
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
/// Ordered by the issue that added each, which is also the order they were
/// dispatched in before:
///
/// * `composio_execute` — #441, keyed on the action slug.
/// * `web_fetch` — #673, keyed on the URL's host.
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
/// and the cautious answer had to win for both (issue #441). Three more tools
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
/// The classification is [`Standing::ScopedGrantable`] **only when a host can be
/// read**, and [`Standing::PerCall`] otherwise. That coupling is load-bearing
/// rather than tidy: a grant is minted with the scope
/// [`standing_scope_of`] returns, and
/// [`StandingGrant::admits_scope`](crate::runtime::grants::StandingGrant::admits_scope)
/// treats an unscoped grant as admitting **everything**. Were an unreadable URL
/// still grantable, approving one card would mint a grant admitting every host
/// on earth. Tying the two answers to one function makes that unrepresentable.
///
/// [`Reach`] is untouched: a fetch still reaches outside the company, so it
/// still parks under `supervised`, and — via [`Standing::ScopedGrantable`] —
/// still parks under `auto`. What changes is only that the operator now has
/// something bounded to say yes to.
fn web_fetch_consequence(args: &serde_json::Value) -> Consequence {
    Consequence {
        group: EffectGroup::Other,
        reach: Reach::Consequence,
        standing: match web_fetch_scope_of(args) {
            Some(_) => Standing::ScopedGrantable,
            None => Standing::PerCall,
        },
    }
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
    use openhuman_core::openhuman::security::{CommandClass, SecurityPolicy};

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

pub fn standing_scope_of(tool: &str, args: &serde_json::Value) -> Option<String> {
    // Issue #673: the same one-function rule the Composio arm follows, for the
    // same reason — the mint side and the live call must read the host with the
    // identical code, or a grant could be minted that never matches its own tool.
    if tool.eq_ignore_ascii_case(WEB_FETCH) {
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
    use openhuman_core::openhuman::memory::sync::composio::providers::{
        catalog_for_toolkit, toolkit_from_slug,
    };
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
    use openhuman_core::openhuman::memory::sync::composio::providers::{
        ToolScope, catalog_for_toolkit, find_curated, toolkit_from_slug,
    };
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
mod tests {
    use super::*;
    use serde_json::json;

    fn c(tool: &str) -> Consequence {
        consequence_of(tool, &json!({}))
    }

    // ── hosting (issue #1079) ───────────────────────────────────────────────

    /// The three tools openhuman's `hosting/README.md` labels "Read-only." ask
    /// the provider what exists and what it did. Asking whether a build
    /// finished must not cost an operator an approval.
    #[test]
    fn a_hosting_read_does_not_park() {
        for tool in [
            "hosting_deployment_status",
            "hosting_list_sites",
            "hosting_analytics",
            "hosting_list_deployments",
            "hosting_domain_status",
        ] {
            let consequence = c(tool);
            assert_eq!(
                consequence.reach,
                Reach::Nothing,
                "`{tool}` only reads the provider"
            );
            assert!(
                !consequence.parks_under_auto(),
                "`{tool}` must not interrupt anybody"
            );
        }
    }

    /// The outward effects still park. Without this the downgrade above would
    /// pass against a table that stopped gating the whole namespace.
    #[test]
    fn a_hosting_effect_still_parks() {
        for tool in [
            "hosting_launch_site",
            "hosting_add_domain",
            "hosting_set_env",
            "hosting_rollback",
        ] {
            let consequence = c(tool);
            assert_eq!(
                consequence.reach,
                Reach::Consequence,
                "`{tool}` changes provider state"
            );
            assert!(
                consequence.parks_under_auto(),
                "`{tool}` must still park under auto"
            );
        }
    }

    /// **The label inversion this fixes.** Before declaring these, the fallback's
    /// `undeclared_group` matched on substrings: `hosting_launch_site` — the
    /// actual public deployment — contains no `deploy`/`publish`/`post` and fell
    /// through to `Other`, while `hosting_deployment_status` — a read — contains
    /// `deploy` and came back `Publish`. The operator's card described the
    /// risky call as nothing in particular and the harmless one as a publish.
    #[test]
    fn the_deployment_is_labelled_publish_and_the_status_read_is_not() {
        assert_eq!(c("hosting_launch_site").group, EffectGroup::Publish);
        assert_eq!(c("hosting_add_domain").group, EffectGroup::Publish);
        assert_ne!(
            c("hosting_deployment_status").group,
            EffectGroup::Publish,
            "a status read must not announce itself as a deployment"
        );
    }

    /// `hosting_set_env` is `Other`, not `Publish`, and the tool's own
    /// description is why: "The site must be redeployed afterwards for a
    /// build-time variable to take effect." It changes what the NEXT deployment
    /// serves and does not itself deploy, so a `Publish` card would tell an
    /// operator a deployment is happening when none is.
    #[test]
    fn setting_env_is_not_labelled_as_a_deployment() {
        let consequence = c("hosting_set_env");
        assert_eq!(consequence.group, EffectGroup::Other);
        assert_eq!(consequence.reach, Reach::Consequence);
    }

    /// Every hosting tool answers from the table, not from `undeclared()`.
    ///
    /// This is the regression guard for the mechanism itself: the fallback's
    /// `READ_ONLY_PREFIXES` are matched with `name.starts_with`, so a
    /// `hosting_`-prefixed read can never match one and the fallback cannot
    /// classify any of these correctly. If a row is dropped, the tool silently
    /// returns to that fallback rather than erroring — so the coverage is
    /// asserted directly.
    ///
    /// **This list is a floor, not the coverage guard, and issue #913 is why
    /// the difference matters.** A hardcoded list only fails when a row is
    /// *removed*; it says nothing when the vendor pin *adds* a tool. That is
    /// exactly what happened — `hosting_rollback`, `hosting_list_deployments`
    /// and `hosting_domain_status` arrived in the pin, were wired onto live
    /// agents by `hosting_tools`, and this test stayed green while all three
    /// fell through to `undeclared()`. The exhaustive check is
    /// `every_wired_hosting_tool_is_declared` in
    /// [`crate::harness::built_in::hosting`], which enumerates the belt itself;
    /// it lives there because it needs the `openhuman` feature, and this file
    /// compiles in lanes that do not have it. Keep both: this one holds in
    /// every lane, that one is exhaustive in the lane that ships.
    #[test]
    fn every_hosting_tool_is_declared() {
        let declared: std::collections::BTreeSet<&str> = declared_tools().collect();
        for tool in [
            "hosting_deployment_status",
            "hosting_list_sites",
            "hosting_analytics",
            "hosting_list_deployments",
            "hosting_domain_status",
            "hosting_launch_site",
            "hosting_add_domain",
            "hosting_set_env",
            "hosting_rollback",
        ] {
            assert!(
                declared.contains(tool),
                "`{tool}` fell back to `undeclared()`, where the `hosting_` prefix \
                 defeats the read test — declare it in DECLARED"
            );
        }
    }

    /// The mechanism, pinned on a name that is *not* declared: a namespaced read
    /// still cannot be seen by the prefix test.
    ///
    /// Kept as documentation of why declaring is the fix rather than teaching
    /// the fallback to split on `_`. Widening that test would extend trust to
    /// tools no belt registered and no reviewer saw, and would turn a
    /// fail-closed miss into a fail-open one.
    #[test]
    fn the_fallback_cannot_see_a_read_verb_behind_a_namespace() {
        assert_eq!(
            c("hosting_list_something_undeclared").reach,
            Reach::Consequence,
            "an undeclared namespaced read gates — inconvenient, and the safe direction"
        );
        assert_eq!(
            c("list_something_undeclared").reach,
            Reach::Nothing,
            "the same verb at the front is seen, which is what makes the namespace the problem"
        );
    }

    // -----------------------------------------------------------------------
    // Issue #673: a host-scoped fetch grant, and the `auto` line it must not
    // cross
    // -----------------------------------------------------------------------

    /// A `web_fetch` call carrying a real URL, as the policy layer sees it.
    fn fetching(url: &str) -> serde_json::Value {
        json!({ WEB_FETCH_URL_KEY: url })
    }

    /// **The entire reason [`Standing::ScopedGrantable`] exists, as a rule.**
    ///
    /// The naive fix for #673 — declaring `web_fetch` [`Standing::Grantable`] to
    /// obtain a scoped grant — was tried and rejected: because
    /// [`Consequence::parks_under_auto`] read `is_grantable`, it also stopped the
    /// tool parking under `auto`, for every agent, with no card and therefore no
    /// scope ever consulted. `the_auto_tier_line_is_pinned_tool_by_tool` catches
    /// that, and this states the invariant that must hold for the *repair* not to
    /// re-open the same hole from the other side.
    ///
    /// Exhaustive over [`Reach`] rather than sampled, because the variant is
    /// argument-classified and so appears nowhere in [`DECLARED`] for a table
    /// walk to find.
    #[test]
    fn a_scoped_grantable_call_is_delegable_but_never_unattended_under_auto() {
        assert!(
            Standing::ScopedGrantable.is_grantable(),
            "the point of the variant is that an operator CAN delegate it"
        );
        assert!(
            !Standing::ScopedGrantable.runs_unattended_under_auto(),
            "and that it still parks under auto — collapsing these two answers \
             back together is exactly the bug issue #673 fixed"
        );

        for reach in [
            Reach::Nothing,
            Reach::Money,
            Reach::ExternalRead,
            Reach::Consequence,
        ] {
            let verdict = Consequence {
                group: EffectGroup::Other,
                reach,
                standing: Standing::ScopedGrantable,
            };
            assert_eq!(
                verdict.parks_under_auto(),
                reach.parks_under_supervision(),
                "a scoped-grantable tool must park under `auto` wherever it parks \
                 under `supervised` — {reach:?} disagreed"
            );
        }
    }

    /// The same rule walked over the declaration table, so a tool that becomes
    /// scoped-grantable later is covered without editing this test.
    ///
    /// The `seen` counter is the point: every tool here is probed with arguments
    /// rich enough to reach the argument-classified branches, and a walk that
    /// found no scoped-grantable verdict at all would pass while asserting
    /// nothing.
    #[test]
    fn every_scoped_grantable_tool_in_the_table_parks_under_auto() {
        let probe = json!({
            WEB_FETCH_URL_KEY: "https://docs.rs/serde",
            COMPOSIO_ACTION_KEY: "GITHUB_GET_A_REPOSITORY",
        });
        let mut seen = 0;
        for tool in declared_tools() {
            let verdict = consequence_of(tool, &probe);
            if verdict.standing == Standing::ScopedGrantable {
                seen += 1;
                assert!(
                    verdict.parks_under_auto(),
                    "`{tool}` is scoped-grantable and must still park under auto"
                );
            }
        }
        assert!(
            seen > 0,
            "the walk reached no scoped-grantable tool, so it proved nothing"
        );
    }

    /// A fetch of a named host is grantable and scoped to that host; the same
    /// call with an unreadable URL is neither.
    ///
    /// The second half is not tidiness. A grant is minted with whatever
    /// `standing_scope_of` returned, and an unscoped grant admits *everything*
    /// (`StandingGrant::admits_scope`), so a URL-less call that stayed grantable
    /// would let one approval mint a grant over every host on earth. The two
    /// answers come from one function precisely so that cannot be represented.
    #[test]
    fn a_fetch_is_grantable_only_when_its_host_can_be_read() {
        let verdict = consequence_of(WEB_FETCH, &fetching("https://docs.rs/serde/latest"));
        assert_eq!(verdict.standing, Standing::ScopedGrantable);
        assert_eq!(
            standing_scope_of(WEB_FETCH, &fetching("https://docs.rs/serde/latest")),
            Some("https://docs.rs".to_string())
        );

        for unreadable in [
            json!({}),                         // no url at all
            fetching("not-a-url"),             // no scheme
            fetching("file:///etc/passwd"),    // not http(s)
            fetching("ftp://example.com/x"),   // not http(s)
            fetching("https://"),              // no host
            fetching("https://:8080/"),        // a port naming no host
            fetching("https://exa mple.com/"), // outside the host alphabet
        ] {
            let verdict = consequence_of(WEB_FETCH, &unreadable);
            assert_eq!(
                verdict.standing,
                Standing::PerCall,
                "an unreadable URL must not be grantable: {unreadable}"
            );
            assert_eq!(
                standing_scope_of(WEB_FETCH, &unreadable),
                None,
                "{unreadable}"
            );
        }
    }

    /// **The userinfo trap.** `https://docs.rs@evil.example/` fetches
    /// `evil.example` — everything before the last `@` is credentials. A reader
    /// that took the authority left-to-right would hand this call the `docs.rs`
    /// scope and let any URL claim any grant, so it is asserted rather than
    /// trusted to the shape of the code.
    #[test]
    fn credentials_in_a_url_cannot_claim_another_hosts_scope() {
        assert_eq!(
            standing_scope_of(WEB_FETCH, &fetching("https://docs.rs@evil.example/x")),
            Some("https://evil.example".to_string())
        );
        // Two `@` — the host is still what follows the LAST one.
        assert_eq!(
            standing_scope_of(WEB_FETCH, &fetching("https://a@b@evil.example/x")),
            Some("https://evil.example".to_string())
        );
        // A backslash is a path separator per WHATWG, so it terminates the
        // authority exactly as `/` does. Without this split the URL would read
        // `docs.rs` as the host and let `evil.example` satisfy a grant minted
        // for `docs.rs` — the userinfo trap re-opened through a delimiter this
        // split never handled.
        assert_eq!(
            standing_scope_of(WEB_FETCH, &fetching("https://evil.example\\@docs.rs/x")),
            Some("https://evil.example".to_string())
        );
    }

    /// The host key is exact. Neither a suffix nor a subdomain of a granted host
    /// resolves to that host's scope, because both are hosts the operator never
    /// read on the card.
    #[test]
    fn the_host_key_admits_neither_a_suffix_nor_a_subdomain() {
        let granted = standing_scope_of(WEB_FETCH, &fetching("https://docs.rs/")).unwrap();
        for impostor in [
            "https://evil-docs.rs/", // suffix match would admit this
            "https://evil.docs.rs/", // subdomain match would admit this
            "https://docs.rs.evil/", // prefix match would admit this
            "http://docs.rs/",       // the cleartext twin
            "https://docs.rs:8443/", // a different service on the same host
        ] {
            assert_ne!(
                standing_scope_of(WEB_FETCH, &fetching(impostor)),
                Some(granted.clone()),
                "`{impostor}` must not resolve to the scope granted for docs.rs"
            );
        }
    }

    /// A host is case-insensitive and `:443` is what `https` means, so these
    /// spellings must produce one scope — otherwise a grant an operator approved
    /// stops matching the very next call and the feature reads as broken.
    #[test]
    fn one_host_in_two_spellings_is_one_scope() {
        // The concrete scope is asserted first, not just the spellings against
        // each other: a regression that returned `None` for every spelling would
        // otherwise satisfy this test vacuously.
        let canonical = standing_scope_of(WEB_FETCH, &fetching("https://docs.rs/serde"));
        assert_eq!(canonical.as_deref(), Some("https://docs.rs"));
        for spelling in [
            "HTTPS://Docs.RS/Serde",
            "https://docs.rs:443/serde",
            "https://user:pw@docs.rs/serde",
        ] {
            assert_eq!(
                standing_scope_of(WEB_FETCH, &fetching(spelling)),
                canonical,
                "`{spelling}` names the same service and must share its scope"
            );
        }
    }

    /// **The bypass class this key must never re-open.**
    ///
    /// Found in review of this change. The key was originally derived by reading
    /// the URL string here — splitting the authority on `/`, `?` and `#`, then
    /// taking whatever followed the last `@`. But `\` is *also* a path separator
    /// in an http(s) URL, so `https://evil.com\@docs.rs/` is fetched from
    /// `evil.com` while that reader minted a grant for `docs.rs`: an operator
    /// approving "fetch from docs.rs" would have authorised `evil.com`. Tab,
    /// newline and CR are stripped before parsing and were a second family of
    /// the same bug.
    ///
    /// The repair was to stop hand-parsing and derive the key from [`url::Url`],
    /// the parser `reqwest` uses to perform the fetch — so there is no second
    /// reader left to disagree with. This test is what keeps that true: it
    /// consults `url` **independently** for the host each URL really resolves to,
    /// and asserts the scope names that host. It is therefore not a tautology
    /// restating the implementation — it is a cross-check that fails the moment
    /// anyone reintroduces a bespoke reader, however carefully written.
    ///
    /// Both directions are in the table on purpose. A key naming a host the fetch
    /// will *not* reach lets a grant be spent elsewhere; a key naming a host the
    /// operator did not see on the card is the same confusion pointed the other
    /// way. Neither is acceptable.
    #[test]
    fn the_scope_names_the_host_the_fetching_client_will_actually_use() {
        for (raw, really_fetches) in [
            // The reported case: `\` terminates the authority, so everything
            // after it — including the `@` — is path.
            (r"https://evil.com\@docs.rs/", "evil.com"),
            // The same trick pointed the other way.
            (r"https://docs.rs\@evil.com/", "docs.rs"),
            // Mixed separators, both orders.
            (r"https://docs.rs\/@evil.com/", "docs.rs"),
            (r"https://docs.rs/\@evil.com", "docs.rs"),
            // Stripped-whitespace family: removed before parsing, so the `@`
            // that survives is a real userinfo delimiter.
            ("https://docs.rs\t@evil.com/", "evil.com"),
            ("https://docs.rs\n@evil.com/", "evil.com"),
            ("https://evil.com@\tdocs.rs/", "docs.rs"),
            // Stripping plus a backslash, together.
            ("https://\revil.com\\@docs.rs/", "evil.com"),
            // The plain userinfo case that was already defended.
            ("https://docs.rs@evil.example/x", "evil.example"),
        ] {
            // The fetching client's own answer, consulted here rather than
            // assumed — if a `url` upgrade ever changes it, this fails loudly
            // instead of the fixture quietly going stale.
            let client_host = url::Url::parse(raw)
                .unwrap_or_else(|e| panic!("fixture must parse: {raw:?}: {e}"))
                .host_str()
                .unwrap_or_else(|| panic!("fixture must name a host: {raw:?}"))
                .to_string();
            assert_eq!(
                client_host, really_fetches,
                "fixture drift: {raw:?} no longer resolves where this table says"
            );

            assert_eq!(
                standing_scope_of(WEB_FETCH, &fetching(raw)).as_deref(),
                Some(format!("https://{really_fetches}").as_str()),
                "the grant scope for {raw:?} must name the host the fetch reaches"
            );
        }
    }

    /// The `auto` line, named tool by tool and taken from the whole table
    /// rather than a sample (issue #560).
    ///
    /// [`Consequence::parks_under_auto`] is easy to check as a predicate; what
    /// an operator actually feels is *which tools* stopped asking. And since
    /// #560, [`Standing::Grantable`] decides two things at once — may be
    /// delegated to one teammate, **and** runs unattended for everyone under
    /// `auto` — so an edit loosening one tool for a delegation reason moves it
    /// across this line as a side effect.
    ///
    /// This walks [`declared_tools`], so a tool joining or leaving the
    /// unattended set fails here and has to be named deliberately. The
    /// predicate test alone would not notice.
    #[test]
    fn the_auto_tier_line_is_pinned_tool_by_tool() {
        // The whole of what `auto` changes: parks for an operator under
        // `supervised`, runs unattended under `auto`. Every entry is the
        // agent's own sandbox or this company's own memory — nothing here
        // leaves the building or spends money.
        const MOVED_BY_AUTO: &[&str] = &[
            "apply_patch",
            "csv_export",
            "edit",
            "file_write",
            "memory_store",
            // Issue #903, and the one entry that is not the agent's private
            // sandbox: it writes into the company's shared workspace. Declared
            // deliberately. A publish still reaches no counterparty and no
            // address, and the artifact chain versions it, so the company can
            // undo it alone — the two properties every other name here has.
            // What it buys is that a finished deliverable reaches the operator
            // without a per-file decision, which is the whole point of `auto`.
            "publish_artifact",
        ];

        let crossers = |args: &serde_json::Value| {
            let mut moved: Vec<&str> = declared_tools()
                .filter(|tool| {
                    let verdict = consequence_of(tool, args);
                    verdict.reach.parks_under_supervision() && !verdict.parks_under_auto()
                })
                .collect();
            moved.sort_unstable();
            moved
        };

        assert_eq!(
            crossers(&json!({})),
            MOVED_BY_AUTO,
            "a tool crossed the `auto` line. If that is intended, say so here — \
             `Standing::Grantable` now also means 'runs unattended for every agent \
             while the company sits in auto', which is wider than the standing \
             grant the field is named for"
        );

        // The same walk with arguments (issue #673). Two tools are classified
        // from their arguments rather than their name, so the empty-args walk
        // above cannot see the verdict they actually produce in service — a
        // `web_fetch` reading a real URL is the grantable shape, and the bare
        // name is not. Without this the line would be pinned only for the tools
        // whose classification the walk happens to be able to reach, and a
        // `web_fetch` loosened to `Standing::Grantable` would cross this line
        // unobserved.
        assert_eq!(
            crossers(&json!({
                WEB_FETCH_URL_KEY: "https://docs.rs/serde",
                COMPOSIO_ACTION_KEY: "GITHUB_GET_A_REPOSITORY",
            })),
            MOVED_BY_AUTO,
            "a tool crossed the `auto` line once its arguments were read. An \
             outward fetch must be delegable to one teammate (`ScopedGrantable`) \
             WITHOUT running unattended for everyone under `auto` — see issue #673"
        );

        // The other direction, spelled out: the tools an operator would be
        // most alarmed to find running unattended still park.
        for tool in [
            "shell",
            "http_request",
            "git_operations",
            "workspace_write",
            "workspace_delete",
            "workspace_rename",
            "media_generate_image",
            "media_generate_video",
            "mcp_call_tool",
            "run_workflow",
            // Issue #661 (M7): removing a workflow takes its whole revision
            // history with it, so there is nothing to restore afterwards. Its
            // read and update siblings deliberately do NOT park (see `DECLARED`)
            // — naming the one that does is how that split stays a decision.
            "delete_workflow",
            "some_tool_nobody_declared",
        ] {
            assert!(
                c(tool).parks_under_auto(),
                "`{tool}` leaves the company, spends money, or cannot be seen into — \
                 it must still park under auto"
            );
        }

        // And the boundary `auto` deliberately does not draw: a billed read is
        // not a park. `web_search` runs under `supervised` because openhuman
        // resolves a `RequireApproval` inline — a parked search never happens —
        // and `auto` must not be stricter than the tier it replaces. The daily
        // cap is what holds spend.
        assert!(!c("web_search").parks_under_auto());
        assert!(c("web_search").reach.costs_money());

        // The other boundary `auto` deliberately does not draw (issue #903):
        // handing a finished file to the operator. `publish_artifact` changes
        // state, so it keeps `Reach::Consequence` and still parks under
        // `supervised` — but it reaches no counterparty and no address, writes
        // only into the company's own workspace and artifact chain, and is
        // versioned, so it is reversible by the company alone. Parking it under
        // `auto` made every deliverable wait on a human: one 9-node pipeline
        // run generated 15 of these.
        assert!(
            !c("publish_artifact").parks_under_auto(),
            "handing a file to the operator does not leave the company"
        );
        assert!(
            c("publish_artifact").reach.parks_under_supervision(),
            "a supervised desk must still see a publish before it lands"
        );
        assert!(
            !c("publish_artifact").reach.costs_money(),
            "a publish is not a spend, so the daily cap must not bill for it"
        );
    }

    /// The argument-classified half of the same line: a Composio read runs
    /// unattended under `auto`, a send does not — and the cautious fallback
    /// keeps an unclassified action on the parking side.
    #[test]
    #[cfg(feature = "openhuman")]
    fn the_auto_line_reads_composio_arguments_not_the_tool_name() {
        let auto = |slug: &str| {
            consequence_of(COMPOSIO_EXECUTE, &json!({ "tool": slug })).parks_under_auto()
        };
        assert!(!auto("GITHUB_LIST_PULL_REQUESTS"), "a catalogue read runs");
        assert!(auto("GMAIL_SEND_EMAIL"), "a send still parks");
        assert!(
            auto("GITHUB_INVENT_A_NEW_VERB"),
            "an action nobody has classified is a send, in this tier too"
        );
    }

    #[test]
    fn the_table_names_each_tool_once() {
        let mut seen: Vec<&str> = DECLARED.iter().map(|d| d.tool).collect();
        let before = seen.len();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(before, seen.len(), "a tool is declared twice: {seen:?}");
        for entry in DECLARED {
            assert_eq!(
                entry.tool,
                entry.tool.to_ascii_lowercase(),
                "declarations are matched lowercased, so `{}` could never be found",
                entry.tool
            );
            assert_ne!(
                entry.tool, COMPOSIO_EXECUTE,
                "`composio_execute` is classified from its arguments, not the table"
            );
        }
    }

    /// Issue #444's headline: the three broadest capabilities in the system
    /// were grantable for up to a week because their names carry no
    /// consequence word. They are named tools now, and named tools are
    /// classified by what they reach.
    #[test]
    fn arbitrary_code_addresses_and_operator_guidance_are_never_grantable() {
        for tool in [
            "shell",
            "http_request",
            "curl",
            "web_fetch",
            "workspace_create",
            "workspace_write",
            "workspace_delete",
            "workspace_rename",
            "git_operations",
            "run_workflow",
            "mcp_call_tool",
            "mcp_registry_tool_call",
        ] {
            assert_eq!(
                c(tool).standing,
                Standing::PerCall,
                "`{tool}` can reach further than a standing grant can honestly describe"
            );
        }
    }

    /// The other half of #444: a tool nobody has classified must not inherit
    /// the longest permission available just by landing in the residual bucket.
    #[test]
    fn an_undeclared_tool_is_never_grantable() {
        assert_eq!(c("some_tool_nobody_declared").standing, Standing::PerCall);
        // Including one that reads — not grantable is about standing, not about
        // whether it parks.
        let read = c("list_something_undeclared");
        assert_eq!(read.reach, Reach::Nothing);
        assert_eq!(read.standing, Standing::PerCall);
    }

    /// Issue #441: the consequence of a Composio call is a property of the
    /// action, not of the one tool name every action arrives under.
    ///
    /// Gated on the harness feature because the read verdict comes from the
    /// vendored provider catalogue, which is only linked in there — the
    /// default build's cautious fallback is pinned separately by
    /// [`without_the_catalogue_every_composio_action_is_a_send`].
    #[test]
    #[cfg(feature = "openhuman")]
    fn a_composio_read_is_grantable_and_a_send_is_not() {
        let read = consequence_of(
            COMPOSIO_EXECUTE,
            &json!({ "tool": "GITHUB_LIST_PULL_REQUESTS" }),
        );
        assert_eq!(read.group, EffectGroup::Other);
        assert_eq!(read.standing, Standing::Grantable);
        // …and since issue #559 it does not park: it reaches GitHub, so
        // `readonly` still denies it, but it changes nothing and costs nothing,
        // so `supervised` runs it.
        assert_eq!(read.reach, Reach::ExternalRead);

        let send = consequence_of(COMPOSIO_EXECUTE, &json!({ "tool": "GMAIL_SEND_EMAIL" }));
        assert_eq!(send.group, EffectGroup::Send);
        assert_eq!(send.standing, Standing::PerCall);
        assert_eq!(send.reach, Reach::Consequence);
    }

    /// The cautious direction, four ways: an action whose slug says nothing
    /// this module recognises, a missing slug, a slug of the wrong type, and
    /// arguments with no slug at all.
    ///
    /// Narrowed by issue #1818, and the narrowing is the point. `..._LIST_...`
    /// left this list because a slug that names a read verb is no longer
    /// "unclassifiable" — see
    /// [`a_drifted_read_runs_instead_of_parking_as_spend`]. What stayed is
    /// every shape that offers no evidence either way, and for those the answer
    /// is the same one it always was.
    #[test]
    fn an_unrecognised_composio_action_is_a_send() {
        for args in [
            json!({ "tool": "GITHUB_INVENT_A_NEW_VERB" }),
            json!({ "tool": "NOTAREALTOOLKIT_DO_SOMETHING" }),
            json!({ "tool": "" }),
            json!({ "tool": 7 }),
            json!({ "arguments": { "owner": "acme" } }),
            json!({}),
        ] {
            let verdict = consequence_of(COMPOSIO_EXECUTE, &args);
            assert_eq!(
                verdict.group,
                EffectGroup::Send,
                "an unclassifiable action must read as a send: {args}"
            );
            assert_eq!(verdict.standing, Standing::PerCall, "{args}");
        }
    }

    /// **Issue #1818, the headline.** A Composio *read* whose slug the curated
    /// catalogue cannot place no longer parks under a card that says it leaves
    /// the company or spends money.
    ///
    /// `GITHUB_ISSUES_LIST_FOR_REPO` is the live evidence from the issue: the
    /// same GitHub operation as the curated `GITHUB_LIST_REPOSITORY_ISSUES`,
    /// under Composio's `operationId`-derived spelling. Before this it was a
    /// `Send + Consequence + PerCall` — parked, labelled as spend, and
    /// un-grantable, so the desk could not even be unblocked by consenting once.
    ///
    /// The two halves that must both hold: the reach is the one a read
    /// deserves, and the group never says spend.
    #[test]
    #[cfg(feature = "openhuman")]
    fn a_drifted_read_runs_instead_of_parking_as_spend() {
        for slug in [
            // Every slug here is absent from the curated catalogue — checked,
            // not assumed: `a_drifted_read_is_a_miss_and_its_curated_twin_is_not`
            // pins the first one as an `UncuratedAction`, and a slug that
            // quietly gained a curated entry would make this test pass for the
            // wrong reason.
            "GITHUB_ISSUES_LIST_FOR_REPO",
            "GITHUB_GET_ISSUE",
            "GITHUB_LIST_ISSUES",
            "SLACK_SEARCH_MESSAGES",
            "NOTION_SEARCH_PAGES",
        ] {
            assert!(
                matches!(
                    composio_catalog_lookup(slug),
                    CatalogLookup::UncuratedAction { .. } | CatalogLookup::UnknownToolkit { .. }
                ),
                "`{slug}` is curated now, so it no longer exercises the fallback — \
                 pick another uncurated read"
            );
            let verdict = consequence_of(COMPOSIO_EXECUTE, &json!({ "tool": slug }));
            assert_eq!(
                verdict.reach,
                Reach::ExternalRead,
                "`{slug}` names a read verb; it must not be priced as a send"
            );
            assert_eq!(
                verdict.group,
                EffectGroup::Other,
                "`{slug}` is a read — the card must not say spend"
            );
            assert!(
                !verdict.parks_under_auto(),
                "`{slug}` must not stall an auto desk"
            );
            assert!(
                !verdict.reach.parks_under_supervision(),
                "`{slug}` must not interrupt a supervised operator either"
            );
            // The tier that still says no, and should: `readonly` promises the
            // desk reaches into nobody's account, drifted slug or not.
            assert!(verdict.reach.denied_under_readonly(), "{slug}");
        }
    }

    /// The other half of #1818: an inferred read runs, but it can never be
    /// minted into a standing grant.
    ///
    /// A curated read is `Grantable` because a person classified it. A verb is
    /// evidence, not a classification, and a standing grant outlives the call
    /// it was cut from — so the guess gets the narrow reading, which expires
    /// with the turn. `PerCall` costs nothing here precisely because
    /// `ExternalRead` does not park: there is no approval to save.
    #[test]
    #[cfg(feature = "openhuman")]
    fn an_inferred_read_is_never_grantable() {
        let inferred = consequence_of(
            COMPOSIO_EXECUTE,
            &json!({ "tool": "GITHUB_ISSUES_LIST_FOR_REPO" }),
        );
        assert_eq!(inferred.standing, Standing::PerCall);
        let curated = consequence_of(
            COMPOSIO_EXECUTE,
            &json!({ "tool": "GITHUB_LIST_REPOSITORY_ISSUES" }),
        );
        assert_eq!(
            curated.standing,
            Standing::Grantable,
            "the curated twin keeps the grant a person's classification earned"
        );
        assert_eq!(
            inferred.reach, curated.reach,
            "they differ on standing and on nothing else — that is the whole distinction"
        );
        assert_eq!(inferred.group, curated.group);
    }

    /// The fallback's own table, stated rather than sampled through
    /// `consequence_of` (issue #1818).
    ///
    /// Both directions matter and they are not symmetric: a `true` here runs
    /// unattended, a `false` is the pre-#1818 park an operator can still
    /// approve. So the read side is checked for the shapes that must run, and
    /// the send side for every shape that must not — including the two the
    /// rules exist for, whole-segment matching and first-verb-wins.
    #[test]
    fn the_verb_fallback_asks_for_evidence_of_a_read() {
        for slug in [
            "GITHUB_ISSUES_LIST_FOR_REPO",
            "GITHUB_LIST_REPOSITORY_ISSUES",
            "GITHUB_GET_A_PULL_REQUEST",
            "GMAIL_FETCH_EMAILS",
            "SLACK_SEARCH_MESSAGES",
            "NOTION_QUERY_DATABASE",
            "linear_list_issues",
            // Whole-segment matching: the mutating verb is a prefix of the
            // object, not the verb. A `contains` rule would send both.
            "GITHUB_LIST_STARGAZERS",
            "GMAIL_LIST_DRAFTS",
        ] {
            assert!(
                composio_slug_reads_by_verb(slug),
                "`{slug}` names a read verb and nothing that mutates"
            );
        }
        for slug in [
            "GMAIL_SEND_EMAIL",
            "GITHUB_CREATE_AN_ISSUE",
            "GITHUB_CREATE_OR_UPDATE_FILE_CONTENTS",
            "STRIPE_CREATE_A_CHARGE",
            "TWITTER_POST_TWEET",
            "GOOGLECALENDAR_QUICK_ADD",
            // No verb either list knows: absence of evidence is a send here,
            // which is the whole difference from upstream's `classify_unknown`.
            "GITHUB_INVENT_A_NEW_VERB",
            "NOTAREALTOOLKIT_DO_SOMETHING",
            "",
            "_",
            // The compound shapes a first-verb-wins rule would let through: a
            // read verb opens each one and a mutation follows it.
            "GITHUB_GET_AND_UPDATE_ISSUE",
            "GMAIL_FIND_OR_CREATE_CONTACT",
            "GMAIL_GET_AND_DELETE_THREAD",
            "GITHUB_LIST_AND_REMOVE_LABELS",
            // The same shape without the conjunction, which is why the object
            // slot gets no exemption. This one is real: a curated **write**.
            "GOOGLESHEETS_FIND_REPLACE",
            "TELEGRAM_ANSWER_CALLBACK_QUERY",
            // Curated, so the fallback never sees it — but if it did, the
            // noun `DRAFT` is indistinguishable from an elided second verb and
            // the rule takes the over-gating side.
            "GMAIL_GET_DRAFT",
        ] {
            assert!(
                !composio_slug_reads_by_verb(slug),
                "`{slug}` is not evidence of a read"
            );
        }
    }

    /// Same verdict, different reasons — and the reasons are now separable
    /// (issue #470). A slug the catalogue cannot place is a legitimate call to
    /// an unclassified action; an argument shape with no readable slug is a
    /// caller bug that the send verdict would otherwise hide, which is exactly
    /// how the `tool_slug` fixtures passed for as long as they did.
    #[test]
    fn a_missing_action_key_is_distinguishable_from_an_unknown_action() {
        assert_eq!(
            composio_action_slug(&json!({ "tool": "NOTAREALTOOLKIT_LIST_THINGS" })),
            Ok("NOTAREALTOOLKIT_LIST_THINGS"),
            "an uncatalogued slug is still a slug — the catalogue, not this reader, \
             is what declines it"
        );
        for (args, expected) in [
            (
                json!({ "tool_slug": "GMAIL_SEND_EMAIL" }),
                ActionKeyMiss::KeyAbsent,
            ),
            (
                json!({ "arguments": { "owner": "acme" } }),
                ActionKeyMiss::KeyAbsent,
            ),
            (json!({}), ActionKeyMiss::KeyAbsent),
            (json!({ "tool": 7 }), ActionKeyMiss::NotAString),
            (json!({ "tool": null }), ActionKeyMiss::NotAString),
            (json!({ "tool": "" }), ActionKeyMiss::Empty),
            (json!({ "tool": "   " }), ActionKeyMiss::Empty),
            (json!("GMAIL_SEND_EMAIL"), ActionKeyMiss::NotAnObject),
            (json!(null), ActionKeyMiss::NotAnObject),
        ] {
            assert_eq!(composio_action_slug(&args), Err(expected), "{args}");
            // …and the verdict is unchanged by any of it: the log line is the
            // only thing that differs, so this can never loosen a decision.
            assert_eq!(
                consequence_of(COMPOSIO_EXECUTE, &args).group,
                EffectGroup::Send,
                "{args}"
            );
        }
    }

    /// A grant scope is read through the same reader, so a call whose slug the
    /// classifier could not find cannot resolve a toolkit either — `None`, and
    /// a scoped grant refuses to admit `None`.
    #[test]
    fn an_unreadable_action_key_resolves_no_grant_scope() {
        for args in [
            json!({ "tool_slug": "GITHUB_LIST_PULL_REQUESTS" }),
            json!({ "tool": "" }),
            json!({ "tool": 7 }),
            json!({}),
        ] {
            assert_eq!(standing_scope_of(COMPOSIO_EXECUTE, &args), None, "{args}");
        }
    }

    /// The seam, pinned from the other side. Without the harness feature the
    /// curated catalogue is not linked in, and the mint path still has to
    /// answer the grantability question — so it answers it the cautious way,
    /// for a read as much as for a send. A default build can only ever see a
    /// `composio_execute` effect replayed from a journal line an openhuman
    /// build wrote, so refusing the standing scope there costs an operator one
    /// approve-once and never a wrong grant.
    #[test]
    #[cfg(not(feature = "openhuman"))]
    fn without_the_catalogue_every_composio_action_is_a_send() {
        // Including one whose verb the #1818 fallback would happily call a
        // read. The fallback is deliberately not consulted here: a build that
        // cannot place `GITHUB_LIST_PULL_REQUESTS` has not earned the right to
        // infer anything, and `CatalogueAbsent` is the arm that says so.
        for slug in [
            "GITHUB_LIST_PULL_REQUESTS",
            "GITHUB_ISSUES_LIST_FOR_REPO",
            "GMAIL_SEND_EMAIL",
        ] {
            assert_eq!(
                composio_catalog_lookup(slug),
                CatalogLookup::CatalogueAbsent,
                "`{slug}` cannot be looked up in a build with no catalogue, and the \
                 record must say that rather than blaming the slug (issue #1818)"
            );
            let verdict = consequence_of(COMPOSIO_EXECUTE, &json!({ "tool": slug }));
            assert_eq!(verdict.group, EffectGroup::Send, "{slug}");
            assert_eq!(verdict.standing, Standing::PerCall, "{slug}");
        }
    }

    /// The seam named from the other side (issue #1818): with the catalogue
    /// linked in, no lookup may ever answer "there is no catalogue".
    ///
    /// `CatalogueAbsent` is a fact about the binary. If it could also arise
    /// from a slug, the operator-facing warning it triggers — *every* Composio
    /// action over-gates in this build — would be a lie told once per stale
    /// slug, and the deployment bug it exists to surface would be unfindable.
    #[test]
    #[cfg(feature = "openhuman")]
    fn a_catalogued_build_never_reports_the_catalogue_absent() {
        for slug in [
            "GITHUB_LIST_PULL_REQUESTS",
            "GITHUB_ISSUES_LIST_FOR_REPO",
            "GMAIL_SEND_EMAIL",
            "NOTAREALTOOLKIT_DO_SOMETHING",
            "noUnderscore",
        ] {
            assert_ne!(
                composio_catalog_lookup(slug),
                CatalogLookup::CatalogueAbsent,
                "`{slug}` was looked up against a catalogue that is present"
            );
        }
    }

    /// Deliberately pinned: upstream's own `classify_unknown` would call
    /// `GITHUB_INVENT_A_NEW_VERB` a read (its fallback arm returns `Read` when
    /// no write verb matches). We do not use it, and this is the test that says
    /// so — if somebody swaps the lookup for the heuristic to "cover more
    /// slugs", the unknown-is-a-send guarantee goes with it.
    #[test]
    #[cfg(feature = "openhuman")]
    fn we_do_not_fall_back_to_the_upstream_read_default() {
        use openhuman_core::openhuman::memory::sync::composio::providers::{
            ToolScope, classify_unknown,
        };
        assert_eq!(
            classify_unknown("GITHUB_INVENT_A_NEW_VERB"),
            ToolScope::Read,
            "upstream's fallback still defaults to read; if this changes the \
             comment above is stale, not the behaviour"
        );
        assert!(!composio_catalog_lookup("GITHUB_INVENT_A_NEW_VERB").is_read());
        // …and issue #1818's fallback did not quietly become that heuristic
        // either. It asks for a read verb; upstream asks only for the absence
        // of a write one, and this slug is the case that separates them.
        assert!(!composio_slug_reads_by_verb("GITHUB_INVENT_A_NEW_VERB"));
        assert_eq!(
            consequence_of(
                COMPOSIO_EXECUTE,
                &json!({ "tool": "GITHUB_INVENT_A_NEW_VERB" })
            )
            .group,
            EffectGroup::Send
        );
    }

    /// **The safety property of the #1818 fallback, over the whole catalogue.**
    ///
    /// The fallback only ever fires on slugs the catalogue *cannot* place, so
    /// there is no direct corpus of them to test against. The curated catalogue
    /// is the next best thing and it is a strong one: ~680 actions a person
    /// hand-classified as `Read` / `Write` / `Admin`. Running the verb rule
    /// over them measures exactly what it would do on the uncurated slugs of
    /// the same shape.
    ///
    /// The two directions are **not** symmetric, so they are asserted
    /// differently:
    ///
    /// * A `Write` or `Admin` the rule calls a read would run unattended.
    ///   That is the bug this test exists to prevent, and it is asserted at
    ///   zero. It found two real vocabulary gaps when it was written —
    ///   `TELEGRAM_ANSWER_CALLBACK_QUERY` (a write, whose `QUERY` is a noun)
    ///   and `GOOGLESHEETS_FIND_REPLACE` (a write, a find-and-replace with the
    ///   conjunction elided) — which is why `ANSWER` and `REPLACE` are in
    ///   `MUTATES`.
    /// * A `Read` the rule calls a send merely parks, which is the pre-#1818
    ///   behaviour. So that side gets a floor rather than a zero: the point is
    ///   to notice a rule that has stopped rescuing anything, not to chase the
    ///   last slug.
    #[test]
    #[cfg(feature = "openhuman")]
    fn the_fallback_never_calls_a_curated_write_a_read() {
        use openhuman_core::openhuman::memory::sync::composio::providers::{
            ToolScope, agent_ready_toolkits, catalog_for_toolkit,
        };

        let entries: Vec<_> = agent_ready_toolkits()
            .into_iter()
            .filter_map(catalog_for_toolkit)
            .flatten()
            .collect();
        assert!(
            entries.len() > 400,
            "the vendored catalogue should be hundreds of actions, found {} — this test \
             is only worth anything if it walks a real corpus",
            entries.len()
        );

        let leaked: Vec<&str> = entries
            .iter()
            .filter(|entry| entry.scope != ToolScope::Read)
            .filter(|entry| composio_slug_reads_by_verb(entry.slug))
            .map(|entry| entry.slug)
            .collect();
        assert!(
            leaked.is_empty(),
            "the verb fallback would run these curated writes unattended: {leaked:?}. \
             Each one names a verb `MUTATES` is missing — add it there rather than \
             narrowing the rule."
        );

        // The other direction: a floor, because over-gating is only a park.
        let reads: Vec<&str> = entries
            .iter()
            .filter(|entry| entry.scope == ToolScope::Read)
            .map(|entry| entry.slug)
            .collect();
        let rescued = reads
            .iter()
            .filter(|slug| composio_slug_reads_by_verb(slug))
            .count();
        assert!(
            rescued * 100 >= reads.len() * 85,
            "the verb rule recognises only {rescued} of {} curated reads. It has stopped \
             rescuing the drifted reads #1818 is about — a read verb was dropped, or a \
             `MUTATES` entry is matching a noun.",
            reads.len()
        );
    }

    /// **Issue #754.** The catalogue miss the whole issue is about, pinned from
    /// both sides of the pair it was reported with.
    ///
    /// `GITHUB_ISSUES_LIST_FOR_REPO` and `GITHUB_LIST_REPOSITORY_ISSUES` are the
    /// same GitHub operation under two naming conventions — Composio's live
    /// `operationId`-derived slug and the curated descriptive one. The second is
    /// classified as a read and runs; the first misses the catalogue and parks.
    ///
    /// The miss is still a miss after issue #1818 — that is what this test is
    /// for, and it is why the two layers are separate functions. #1818 changed
    /// what the classifier *does* with a miss; it must not change what the
    /// catalogue *reports*, or the drift signal #754 exists for would be
    /// silently switched off by the fix that made drift survivable.
    #[test]
    #[cfg(feature = "openhuman")]
    fn a_drifted_read_is_a_miss_and_its_curated_twin_is_not() {
        assert_eq!(
            composio_catalog_lookup("GITHUB_LIST_REPOSITORY_ISSUES"),
            CatalogLookup::Curated { read: true },
            "the curated spelling is a read"
        );
        assert_eq!(
            composio_catalog_lookup("GITHUB_ISSUES_LIST_FOR_REPO"),
            CatalogLookup::UncuratedAction {
                toolkit: "github".to_string()
            },
            "the live spelling of the same operation is a catalogue MISS, and \
             naming it as such is the whole of #754 — the curated name is still \
             the thing to fix even though #1818 stopped the miss from parking"
        );
    }

    /// A curated **write** is not a miss, and telling them apart is what keeps
    /// the signal readable (issue #754).
    ///
    /// Both classify as a send, so a boolean cannot separate them — which is
    /// exactly why the drift was invisible. If every send were reported as a
    /// catalogue miss, `GMAIL_SEND_EMAIL` would drown the handful of slugs that
    /// have actually drifted.
    #[test]
    #[cfg(feature = "openhuman")]
    fn a_curated_write_is_not_reported_as_drift() {
        assert_eq!(
            composio_catalog_lookup("GMAIL_SEND_EMAIL"),
            CatalogLookup::Curated { read: false },
            "a curated send is the gate working, not the catalogue rotting"
        );
    }

    /// A slug whose toolkit has no curated surface is a *different* miss from a
    /// slug its toolkit has never heard of, and the record says which.
    #[test]
    #[cfg(feature = "openhuman")]
    fn an_unrecognised_toolkit_is_its_own_kind_of_miss() {
        assert!(matches!(
            composio_catalog_lookup("NOTAREALTOOLKIT_LIST_THINGS"),
            CatalogLookup::UnknownToolkit { .. }
        ));
    }

    /// Issue #443: the agent persona instructs every agent to call these rather
    /// than answer a capability question from memory. They read local
    /// registration state and reach nothing.
    #[test]
    fn listing_mcp_servers_and_tools_never_parks_but_calling_through_one_does() {
        for tool in [
            "mcp_list_servers",
            "mcp_list_tools",
            "mcp_registry_list_tools",
        ] {
            assert_eq!(c(tool).reach, Reach::Nothing, "`{tool}` reads local state");
        }
        for tool in ["mcp_call_tool", "mcp_registry_tool_call"] {
            assert!(
                c(tool).reach.parks_under_supervision(),
                "`{tool}` can perform any effect the remote server advertises"
            );
        }
    }

    /// The sibling defects the same sweep turned up: four pure reads of the
    /// agent's own workspace that parked because the read-only-prefix rule
    /// keys on the *start* of a name and none of them begins with one.
    ///
    /// `read_workspace_state` was in this list until issue #459 showed it is
    /// not a read at all — see
    /// [`reading_workspace_state_is_classified_with_shell_because_it_runs_git`].
    #[test]
    fn a_workspace_read_never_parks_whatever_its_name_begins_with() {
        for tool in [
            "file_read",
            "glob",
            "grep",
            "image_info",
            "list",
            "memory_recall",
            "workspace_list",
            "workspace_read",
            "workspace_search",
            "media_list_models",
            "composio_list_toolkits",
            "composio_list_connections",
            "composio_list_tools",
        ] {
            assert_eq!(c(tool).reach, Reach::Nothing, "`{tool}` is a read");
        }
    }

    /// Issue #459: `read_workspace_state` is not the read its name promises.
    /// It runs `git` in `{root}/{company}/{agent}/workspace`, and git reads
    /// `.git/config` from that directory — a file the agent's own `file_write`
    /// can author, and one whose keys can name a command to run. Until the
    /// vendored `run_git` refuses untrusted repository config, it is
    /// classified with `shell`.
    ///
    /// The standing assertion is the one that matters most: `file_write` is
    /// grantable, so if this were grantable too, the pair could be handed over
    /// together for a week and the hole would be open for the length of the
    /// grant with nobody watching.
    #[test]
    fn reading_workspace_state_is_classified_with_shell_because_it_runs_git() {
        let verdict = c("read_workspace_state");
        assert_eq!(verdict.reach, Reach::Consequence);
        assert!(
            verdict.reach.parks_under_supervision(),
            "running git under config the agent wrote must reach an operator"
        );
        assert!(
            verdict.reach.denied_under_readonly(),
            "`readonly` promises nothing runs; a config key can name a command"
        );
        assert_eq!(
            verdict.standing,
            Standing::PerCall,
            "a standing grant here would reopen the hole for its whole duration"
        );
        assert_eq!(
            verdict.reach,
            c("shell").reach,
            "it is the `shell` shape and should stay pinned to `shell`'s verdict"
        );
    }

    /// The feature keeps its point: the tools an agent uses to actually do work
    /// in its own sandbox stay grantable, so an operator handing over a stretch
    /// of autonomy is still handing over something useful.
    #[test]
    fn the_agents_own_workspace_writes_stay_grantable() {
        for tool in [
            "file_write",
            "edit",
            "apply_patch",
            "csv_export",
            "memory_store",
        ] {
            let verdict = c(tool);
            assert_eq!(verdict.standing, Standing::Grantable, "`{tool}`");
            // They mutate, so `readonly` must still deny and `supervised` must
            // still park the first call.
            assert!(verdict.reach.parks_under_supervision(), "`{tool}`");
        }
    }

    /// Issue #559, every acceptance criterion in one place — the four verdicts
    /// `ExternalRead` has to give, and the one it must not.
    ///
    /// The three predicates are the whole of the behaviour: `Reach` is never
    /// matched exhaustively outside this module, so adding a variant changes
    /// nothing anywhere until one of these answers differently.
    #[test]
    #[cfg(feature = "openhuman")]
    fn a_composio_read_runs_under_supervision_and_is_still_denied_under_readonly() {
        use crate::policy::test_support::{COMPOSIO_READ_SLUG, composio_read_args};

        let read = consequence_of(COMPOSIO_EXECUTE, &composio_read_args());
        assert_eq!(
            read.reach,
            Reach::ExternalRead,
            "`{COMPOSIO_READ_SLUG}` is tagged `Read` in the vendored catalogue"
        );

        // 1. Under `supervised` it runs, instead of costing the operator a card.
        assert!(
            !read.reach.parks_under_supervision(),
            "reading a mailbox must not interrupt a person"
        );
        // 2. Under `readonly` it is still denied: that tier's contract is that
        //    nothing outside the company is reached at all.
        assert!(read.reach.denied_under_readonly());
        // 3. And it is NOT spend. This is the criterion that rules out reusing
        //    `Reach::Money`, whose `costs_money()` feeds the daily cap — every
        //    page of every mailbox would have counted against it.
        assert!(
            !read.reach.costs_money(),
            "a read is not billed; folding it into `Money` would bill it"
        );
        assert_ne!(read.group, EffectGroup::Spend);

        // 4. The standing answer is unchanged — it stops mattering for
        //    `supervised` now that nothing parks there, but still governs
        //    `readonly` and any tier added later.
        assert_eq!(read.standing, Standing::Grantable);
        assert_eq!(read.group, EffectGroup::Other);
    }

    /// The other half of #559: only the **read** branch moved.
    ///
    /// The `send` binding is shared by the missing-key path and the non-read
    /// path, so these hold structurally — but nothing stops a later edit from
    /// touching that shared binding, which is the whole reason to assert them.
    #[test]
    fn a_composio_send_and_every_unclassifiable_call_still_park() {
        use crate::policy::test_support::{composio_send_args, composio_unclassified_args};

        let cases: [(&str, serde_json::Value); 6] = [
            ("a catalogued send", composio_send_args()),
            ("an uncatalogued action", composio_unclassified_args()),
            (
                "an unrecognised slug in a real toolkit",
                json!({ "tool": "GITHUB_INVENT_A_NEW_VERB" }),
            ),
            ("a non-string `tool`", json!({ "tool": 7 })),
            ("an empty slug", json!({ "tool": "" })),
            ("a missing `tool` key", json!({ "arguments": { "q": "x" } })),
        ];

        for (what, args) in cases {
            let verdict = consequence_of(COMPOSIO_EXECUTE, &args);
            assert_eq!(verdict.reach, Reach::Consequence, "{what}: {args}");
            assert!(
                verdict.reach.parks_under_supervision(),
                "{what} must still park: {args}"
            );
            assert!(verdict.reach.denied_under_readonly(), "{what}: {args}");
            assert_eq!(verdict.group, EffectGroup::Send, "{what}: {args}");
            assert_eq!(verdict.standing, Standing::PerCall, "{what}: {args}");
        }
    }

    /// `ExternalRead` must not leak into the spend cap from any other tool.
    ///
    /// `web_search_is_still_a_priced_call` pins the `Money`→`Spend` direction;
    /// this pins that no *declared* tool picked up the new variant by accident,
    /// so the only thing carrying it is the Composio read branch.
    #[test]
    fn no_declared_tool_claims_the_external_read_bucket() {
        let args = json!({});
        for tool in declared_tools() {
            assert_ne!(
                consequence_of(tool, &args).reach,
                Reach::ExternalRead,
                "`{tool}` is a declared tool; `ExternalRead` is for the Composio \
                 read branch, which is classified from its arguments"
            );
        }
    }

    #[test]
    fn a_metered_read_is_allowed_under_supervision_and_denied_under_readonly() {
        let search = c("web_search");
        assert_eq!(search.reach, Reach::Money);
        assert!(!search.reach.parks_under_supervision());
        assert!(search.reach.denied_under_readonly());
        assert!(search.reach.costs_money());
        assert_eq!(search.group, EffectGroup::Spend);
        assert_eq!(search.standing, Standing::PerCall);
    }

    #[test]
    fn declared_tools_covers_the_table_and_the_argument_classified_tool() {
        let all: Vec<&str> = declared_tools().collect();
        assert!(all.contains(&COMPOSIO_EXECUTE));
        assert!(all.contains(&"shell"));
        // `composio_execute` is the one roster entry with no `DECLARED` row;
        // the other three shadow theirs and are counted once.
        assert_eq!(all.len(), DECLARED.len() + 1);
    }

    /// The two mechanisms **partition** the names the gate knows: every name
    /// [`declared_tools`] yields is answered either from its arguments or from
    /// the table, never neither and never ambiguously (issue #877).
    ///
    /// This is the criterion #877 states in as many words — *"the coverage test
    /// keeps saying which tools answer from arguments and which from the
    /// table"*. The roster is the authority on which side a tool is on, because
    /// it is the same list [`consequence_of`] dispatches through: a tool cannot
    /// be graded by argument without appearing here, and appearing here is what
    /// puts it on the argument side of this assertion.
    #[test]
    fn the_roster_and_the_table_partition_the_known_tool_names() {
        let known: std::collections::BTreeSet<&str> = declared_tools().collect();
        let graded: std::collections::BTreeSet<&str> =
            ARGUMENT_GRADED.iter().map(|(tool, _)| *tool).collect();
        let tabled: std::collections::BTreeSet<&str> = DECLARED
            .iter()
            .map(|d| d.tool)
            .filter(|tool| !graded.contains(tool))
            .collect();

        assert!(
            graded.is_disjoint(&tabled),
            "a name cannot be answered by both mechanisms — the roster shadows \
             the table, so a shadowed row is not on the table side"
        );
        let union: std::collections::BTreeSet<&str> = graded.union(&tabled).copied().collect();
        assert_eq!(
            union, known,
            "every known tool name must sit on exactly one side of the \
             partition; if this fails, a mechanism has grown a name \
             `declared_tools` cannot see"
        );

        // And the sides say what they are, so the failure message above is
        // actionable rather than a set difference.
        for tool in &graded {
            assert!(
                argument_grader(tool).is_some(),
                "`{tool}` is on the roster but `consequence_of` would not \
                 dispatch it"
            );
        }
        for tool in &tabled {
            assert!(
                argument_grader(tool).is_none(),
                "`{tool}` answers from the table but a classifier claims it too"
            );
        }
    }

    /// A classifier added to the roster is enumerated by [`declared_tools`]
    /// **without** anyone remembering to add it there too.
    ///
    /// This is the regression the old shape could not guard: [`declared_tools`]
    /// used to `chain(once(COMPOSIO_EXECUTE))`, naming the single exception by
    /// hand, so a fifth argument-graded tool with no [`DECLARED`] row would
    /// have been dispatched and yet invisible to every test that walks
    /// [`declared_tools`] — #877's "quietly join the coarse side". Driving the
    /// derivation with a synthetic roster is the only way to assert it without
    /// shipping a fake tool.
    #[test]
    fn a_roster_entry_with_no_table_row_is_still_enumerated() {
        const SYNTHETIC: &[(&str, Grader)] = &[("not_a_real_tool", shell_consequence)];
        let names: Vec<&str> = tool_names(DECLARED, SYNTHETIC).collect();
        assert!(
            names.contains(&"not_a_real_tool"),
            "a roster entry with no `DECLARED` row must still be enumerated"
        );
        assert_eq!(
            names.len(),
            DECLARED.len() + 1,
            "and exactly once — the row-less entry is appended, nothing else moves"
        );
    }

    /// A roster entry that shadows a [`DECLARED`] row is counted **once**.
    ///
    /// The union is what makes the partition above meaningful: a concatenation
    /// would double-count `shell`, `web_fetch` and `git_operations`, and every
    /// caller that walks [`declared_tools`] as a set — `always_approve`,
    /// `judgement`, the harness roster — would silently do redundant work over
    /// duplicated names.
    #[test]
    fn a_roster_entry_that_shadows_a_table_row_is_enumerated_once() {
        let names: Vec<&str> = declared_tools().collect();
        for tool in ["shell", WEB_FETCH, GIT_OPERATIONS] {
            assert_eq!(
                names.iter().filter(|name| **name == tool).count(),
                1,
                "`{tool}` holds both a roster entry and a `DECLARED` row and \
                 must be enumerated once"
            );
        }
    }

    /// Every roster name is lower-case and appears once.
    ///
    /// [`consequence_of`] lower-cases the incoming tool name before asking
    /// [`argument_grader`], so a mixed-case entry would be an entry that never
    /// fires — a classifier silently replaced by its table row, which is the
    /// fail-open shape this whole cluster of issues exists to prevent. A
    /// duplicate name would be a second classifier the first one shadows.
    #[test]
    fn the_roster_is_lower_case_and_has_no_duplicates() {
        let mut seen = std::collections::BTreeSet::new();
        for (tool, _) in ARGUMENT_GRADED {
            assert_eq!(
                *tool,
                tool.to_ascii_lowercase(),
                "`{tool}` is matched against a lower-cased name and would never fire"
            );
            assert!(seen.insert(*tool), "`{tool}` appears twice on the roster");
        }
    }

    /// The declaration is matched case-insensitively, the way every other arm
    /// of the gate reads a tool name.
    #[test]
    fn lookup_ignores_case() {
        assert_eq!(c("SHELL").standing, Standing::PerCall);
        assert_eq!(c("Workspace_Read").reach, Reach::Nothing);
        // `composio_execute` is matched by the same lowercasing pass, so an
        // upper-cased tool name still reaches the argument classifier rather
        // than falling through to the undeclared heuristics.
        assert_eq!(
            consequence_of("COMPOSIO_EXECUTE", &json!({ "tool": "GMAIL_SEND_EMAIL" })).group,
            EffectGroup::Send
        );
        #[cfg(feature = "openhuman")]
        assert_eq!(
            consequence_of(
                "COMPOSIO_EXECUTE",
                &json!({ "tool": "github_list_branches" })
            )
            .standing,
            Standing::Grantable,
            "the curated lookup is case-insensitive on the slug too"
        );
    }

    /// The two halves of #457's scoping, exercised **together and directly**
    /// (issue #610).
    ///
    /// [`standing_scope_of`] mints the scope and
    /// [`StandingGrant::admits_scope`] spends it, and since #559 no tier routes
    /// a Composio read through both — see the retention note on
    /// `standing_scope_of`. Each half is pinned on its own elsewhere, and each
    /// of those tests spells the toolkit as its own `"github"` literal. Two
    /// literals in two files are not an agreement: change what
    /// `standing_scope_of` returns and both suites can be made green
    /// separately while the pairing they describe is broken, with no live
    /// caller left to notice.
    ///
    /// So nothing here is written down. Every scope comes out of
    /// `standing_scope_of` and goes straight into a grant or into
    /// `admits_scope`, which makes this a test of whether the two functions
    /// still agree rather than of what either one says.
    #[test]
    #[cfg(feature = "openhuman")]
    fn the_minted_scope_is_the_scope_a_grant_admits() {
        use crate::runtime::grants::{GrantId, StandingGrant};

        let scope_of = |slug: &str| standing_scope_of(COMPOSIO_EXECUTE, &json!({ "tool": slug }));

        let minted = scope_of("GITHUB_LIST_BRANCHES");
        assert!(
            minted.is_some(),
            "a catalogued action must resolve a toolkit, or this test proves nothing"
        );
        // Minted the way the cycle mints one: from the parked effect's payload.
        let grant = StandingGrant {
            id: GrantId::new("g610"),
            agent: "ops".to_string(),
            workflow: None,
            tool: COMPOSIO_EXECUTE.to_string(),
            verdict: crate::ports::types::Verdict::Approve,
            granted_by: crate::ports::types::Actor {
                kind: crate::ports::types::ActorKind::User,
                id: "user-1".to_string(),
            },
            approval_id: crate::ports::types::ApprovalId::new("a610"),
            at_millis: 1_000,
            expires_at_millis: u64::MAX,
            origin_thread: None,
            origin_parent: None,
            origin_task: None,
            scope: minted,
        };

        // A second read from the provider the operator named. Scoped by
        // toolkit and not by slug, so a *different* GitHub action passes.
        assert!(
            grant.admits_scope(scope_of("GITHUB_LIST_PULL_REQUESTS").as_deref()),
            "the operator consented to a provider, not to one action slug"
        );
        // Another provider's read: every other dimension matches and the scope
        // is the one thing that says no.
        assert!(
            !grant.admits_scope(scope_of("GMAIL_FETCH_EMAILS").as_deref()),
            "'read from GitHub' is not consent to read the company's mail"
        );
        // An action the catalogue cannot place resolves to `None`, and a scoped
        // grant refuses `None` rather than guessing permissively.
        assert_eq!(
            scope_of("NOT_A_REAL_TOOLKIT_DO_SOMETHING"),
            None,
            "an unplaceable action must not resolve a toolkit"
        );
        assert!(
            !grant.admits_scope(scope_of("NOT_A_REAL_TOOLKIT_DO_SOMETHING").as_deref()),
            "unknown is a send here too"
        );
        // And the unscoped grant a pre-#457 journal line replays into still
        // admits whatever its `(agent, tool)` pair already admitted.
        let unscoped = StandingGrant {
            scope: None,
            ..grant
        };
        assert!(
            unscoped.admits_scope(scope_of("GMAIL_FETCH_EMAILS").as_deref()),
            "an unscoped grant must keep behaving as it did before scopes existed"
        );
    }

    /// Issue #457: a standing grant on `composio_execute` has to record *which
    /// provider*, because the card said "read from GitHub" and the tool name
    /// says nothing at all.
    #[test]
    #[cfg(feature = "openhuman")]
    fn a_composio_call_is_scoped_to_its_toolkit() {
        assert_eq!(
            standing_scope_of(COMPOSIO_EXECUTE, &json!({ "tool": "GITHUB_LIST_BRANCHES" })),
            Some("github".to_string())
        );
        // A different action in the same toolkit is the same scope — the
        // operator agreed to a provider, not to one slug.
        assert_eq!(
            standing_scope_of(
                COMPOSIO_EXECUTE,
                &json!({ "tool": "GITHUB_LIST_PULL_REQUESTS" })
            ),
            Some("github".to_string())
        );
        // A different provider is a different scope, which is the whole point.
        assert_eq!(
            standing_scope_of(COMPOSIO_EXECUTE, &json!({ "tool": "GMAIL_FETCH_EMAILS" })),
            Some("gmail".to_string())
        );
        // The tool name is matched the same lowercasing way the rest of the
        // gate matches it.
        assert_eq!(
            standing_scope_of(
                "COMPOSIO_EXECUTE",
                &json!({ "tool": "github_list_branches" })
            ),
            Some("github".to_string())
        );
    }

    /// Nothing the catalogue can place is `None`, and `None` is what a scoped
    /// grant refuses — so an unplaceable slug can never ride somebody else's
    /// permission.
    ///
    /// Unchanged by issue #1818, and worth saying why: the verb fallback moved
    /// `..._LIST_...` off the parking path, but it did not give it a scope. A
    /// grant is minted against a *toolkit*, and a toolkit the catalogue has
    /// never heard of is still not one an operator consented to.
    #[test]
    fn an_unplaceable_composio_call_has_no_scope() {
        for args in [
            json!({ "tool": "NOTAREALTOOLKIT_LIST_THINGS" }),
            json!({ "tool": "" }),
            json!({ "tool": 7 }),
            json!({ "arguments": { "owner": "acme" } }),
            json!({}),
        ] {
            assert_eq!(
                standing_scope_of(COMPOSIO_EXECUTE, &args),
                None,
                "nothing to narrow to: {args}"
            );
        }
    }

    /// Every other tool has no scope, and must not grow one by accident: the
    /// name of `file_write` already is the whole of what it can do.
    #[test]
    fn a_tool_whose_name_says_everything_has_no_scope() {
        for tool in [
            "file_write",
            "memory_forget",
            "memory_store",
            "shell",
            "workspace_write",
            "workspace_create",
        ] {
            assert_eq!(
                standing_scope_of(tool, &json!({ "tool": "GITHUB_LIST_BRANCHES" })),
                None,
                "`{tool}` is not a Composio call whatever its arguments say"
            );
        }
    }

    /// The other side of the seam. A default build cannot mint a Composio
    /// standing grant at all (see
    /// `without_the_catalogue_every_composio_action_is_a_send`), so answering
    /// "no scope" here widens nothing — there is no scoped grant to widen.
    #[test]
    #[cfg(not(feature = "openhuman"))]
    fn without_the_catalogue_nothing_carries_a_scope() {
        for slug in ["GITHUB_LIST_BRANCHES", "GMAIL_SEND_EMAIL"] {
            assert_eq!(
                standing_scope_of(COMPOSIO_EXECUTE, &json!({ "tool": slug })),
                None,
                "{slug}"
            );
        }
    }

    /// The literals above and the constants the tools themselves return are two
    /// copies of the same string. This is the test that keeps them one.
    #[test]
    #[cfg(feature = "openhuman")]
    fn the_declared_names_are_the_names_the_tools_return() {
        use crate::harness::{orchestrator, publish, search, workflow_admin, workspace_tools};
        for name in [
            workflow_admin::READ_WORKFLOW_TOOL,
            workflow_admin::UPDATE_WORKFLOW_TOOL,
            workflow_admin::DELETE_WORKFLOW_TOOL,
            orchestrator::QUERY_COMPANY_TOOL,
            orchestrator::SPAWN_TASK_TOOL,
            orchestrator::DELEGATE_TO_DESK_TOOL,
            orchestrator::DELEGATE_TO_TEAMMATE_TOOL,
            orchestrator::ADD_AGENT_TOOL,
            orchestrator::CREATE_WORKFLOW_TOOL,
            orchestrator::ASSIGN_TASK_TOOL,
            orchestrator::REVIEW_TASK_TOOL,
            orchestrator::RUN_WORKFLOW_TOOL,
            publish::PUBLISH_ARTIFACT_TOOL,
            search::WEB_SEARCH_TOOL,
            workspace_tools::WORKSPACE_LIST_TOOL,
            workspace_tools::WORKSPACE_READ_TOOL,
            workspace_tools::WORKSPACE_SEARCH_TOOL,
            workspace_tools::WORKSPACE_CREATE_TOOL,
            workspace_tools::WORKSPACE_WRITE_TOOL,
            workspace_tools::WORKSPACE_RENAME_TOOL,
            workspace_tools::WORKSPACE_DELETE_TOOL,
            crate::harness::composio_catalog::LIST_TOOLS_TOOL,
            crate::harness::composio_catalog::LIST_TOOLKITS_TOOL,
        ] {
            assert!(
                DECLARED.iter().any(|d| d.tool == name),
                "`{name}` is a live tool constant with no declaration"
            );
        }
    }

    /// Where the console keeps the words an operator reads instead of a tool
    /// name. Named once so both the parser and every failure message point at
    /// the same file.
    const LANGUAGE_TS: &str = "frontend/src/lib/language.ts";

    /// One object literal in [`LANGUAGE_TS`], read as `key -> sentence`.
    ///
    /// A line parser rather than the two alternatives, and the reasons are the
    /// same ones that make this a `cargo test` at all:
    ///
    /// * a **checked-in generated manifest** of the declared set would give the
    ///   contract two failure sites and a window between the declaration commit
    ///   and the regenerate commit where nothing is wrong;
    /// * a **CI grep** would have no local signal for the Rust contributor who
    ///   adds the next `Reach::Consequence` line — and that is who introduced
    ///   all three instances of this defect (#372, #551 → #671, now #701).
    ///
    /// It is deliberately literal about the shape it accepts: an object literal
    /// opened by `const <NAME>` on a line ending in `{` and closed by a `};`
    /// line. Anything else panics rather than returning a short list, because a
    /// parser that silently reads nothing turns this test into a green light
    /// for the exact regression it exists to catch — see the vacuity guards in
    /// [`every_consequence_tool_has_a_console_label`].
    ///
    /// It returns pairs rather than keys (issue #743) because the distinctness
    /// half needs the sentences, and one parse feeding both halves is the point:
    /// this test exists because a hand-maintained restatement of the declared
    /// set drifts from it, and a second parser over the same file would be that
    /// same mistake one level down.
    ///
    /// Values are taken literally — the text between the first `:` and the
    /// trailing comma, unquoted. Every entry in both tables is a plain string
    /// literal on one line today; a template literal or a concatenation would
    /// arrive here as its own source text and, being unequal to any other
    /// entry, would pass the distinctness check without asserting anything about
    /// what an operator reads. That is the one shape to reject rather than
    /// tolerate, and the `>=` floors below are what would catch a table that
    /// reshaped into it wholesale.
    fn label_pairs(source: &str, decl: &str) -> Vec<(String, String)> {
        let mut lines = source.lines();
        let opened = lines.any(|line| {
            let line = line.trim_start();
            line.starts_with(&format!("const {decl}")) && line.ends_with('{')
        });
        assert!(
            opened,
            "no `const {decl} … {{` line in {LANGUAGE_TS}. If the table was \
             renamed or reshaped, update this parser — do not delete the test"
        );

        let mut pairs = Vec::new();
        for line in lines {
            let line = line.trim();
            if line == "};" {
                return pairs;
            }
            if line.is_empty() || line.starts_with("//") || line.starts_with('*') {
                continue;
            }
            let Some((key, value)) = line.split_once(':') else {
                continue;
            };
            pairs.push((
                key.trim().trim_matches('"').to_string(),
                value
                    .trim()
                    .trim_end_matches(',')
                    .trim()
                    .trim_matches('"')
                    .to_string(),
            ));
        }
        panic!("`const {decl}` in {LANGUAGE_TS} is never closed by a `}};` line");
    }

    /// Every tool that reaches an operator resolves to a sentence, not to
    /// "Use one of its tools" (issue #701).
    ///
    /// The console's `approvalAction` resolves `EFFECT_LABELS` → `TOOL_LABELS`
    /// → a generic fallback, so a gated tool in neither table asks an operator
    /// to consent to "use one of its tools" — the #372 defect. It has now
    /// recurred three times, and every time the commit that caused it was a
    /// Rust one adding a `Reach::Consequence` declaration with no reason to
    /// open the frontend at all. So the coupling belongs here, next to
    /// [`DECLARED`], where that contributor's `cargo test` reports it.
    ///
    /// Scoped to the whole [`Reach::Consequence`] class rather than to the
    /// per-call subset. The grantable ones (`file_write`, `edit`,
    /// `apply_patch`, `csv_export`) park exactly the same way, and they are
    /// *also* the ones issue #374's Standing-permissions list renders through
    /// `toolAction` with no payload block to disambiguate them. A test scoped
    /// to `Standing::PerCall` would ship blind to four instances of the class
    /// it exists to kill.
    #[test]
    fn every_consequence_tool_has_a_console_label() {
        let source = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/frontend/src/lib/language.ts"
        ));
        // Read as pairs, not keys: the distinctness half below needs the
        // sentences. `label_keys` is the same parse, so the two halves cannot
        // disagree about what the tables hold.
        let effect_labels = label_pairs(source, "EFFECT_LABELS");
        let tool_labels = label_pairs(source, "TOOL_LABELS");
        let effects: Vec<&str> = effect_labels.iter().map(|(k, _)| k.as_str()).collect();
        let tools: Vec<&str> = tool_labels.iter().map(|(k, _)| k.as_str()).collect();

        // Vacuity guards. A parse that quietly returned nothing would report
        // every gated tool as unlabelled — noisy, and therefore self-correcting.
        // A parse that quietly returned *the wrong block* would report none of
        // them, which is the failure that matters: the test would pass forever
        // while the console regressed. These anchors are entries with no reason
        // to move, so a reformat of `language.ts` breaks here loudly instead.
        for anchor in ["payment.send", "workflow.approve"] {
            assert!(
                effects.contains(&anchor),
                "parsed EFFECT_LABELS from {LANGUAGE_TS} without `{anchor}` — \
                 the parser is reading the wrong block, not the table shrinking"
            );
        }
        for anchor in ["shell", "workspace_create"] {
            assert!(
                tools.contains(&anchor),
                "parsed TOOL_LABELS from {LANGUAGE_TS} without `{anchor}` — \
                 the parser is reading the wrong block, not the table shrinking"
            );
        }
        assert!(
            effects.len() >= 15 && tools.len() >= 10,
            "parsed only {} EFFECT_LABELS and {} TOOL_LABELS keys from \
             {LANGUAGE_TS}; both tables are larger than that, so the parser is \
             stopping early",
            effects.len(),
            tools.len()
        );

        let gated: Vec<&str> = declared_tools()
            .filter(|tool| c(tool).reach.parks_under_supervision())
            .collect();

        // The walk's own vacuity guard (issue #743). A distinctness check over
        // an empty or truncated set passes having asserted nothing, which is
        // precisely the fail-open shape the guards above exist to refuse — and
        // the shape that made #706's reproduction wrong by half.
        //
        // A floor rather than an exact count, matching the `>=` idiom above: the
        // gated set is 25 today and grows whenever a `Reach::Consequence` line
        // is declared, so pinning it exactly would fail every such commit for
        // being correct. What must never happen is the walk *shrinking* toward
        // the four hardcoded names this widened.
        assert!(
            gated.len() >= 20,
            "only {} tools were selected as gated; the declaration table holds \
             far more `Reach::Consequence` entries than that, so the walk is \
             selecting almost nothing and everything below it is vacuous",
            gated.len()
        );

        let mut unlabelled: Vec<&str> = gated
            .iter()
            .copied()
            .filter(|tool| !effects.iter().any(|k| k == tool) && !tools.iter().any(|k| k == tool))
            .collect();
        unlabelled.sort_unstable();
        assert!(
            unlabelled.is_empty(),
            "{unlabelled:?} park for an operator but have no entry in either \
             label map in {LANGUAGE_TS}, so their approval card reads \"Use one \
             of its tools\". Add each to TOOL_LABELS — a gated tool's label \
             only ever appears above the payload block, which is what \
             EFFECT_LABELS entries do not assume and why its \
             EFFECT_DONE_LABELS mirror would demand a past-tense twin these \
             kinds never reach"
        );

        // ...and no two of them read the same sentence (issue #743).
        //
        // Checked after the unlabelled walk on purpose: an unlabelled pair would
        // collide here too, on the fallback, and reporting that as "these two
        // read alike" would name the symptom while the assertion above names the
        // cause. Ordering the two is what keeps one failure message honest.
        //
        // Resolved through the console's own rung order — `EFFECT_LABELS` then
        // `TOOL_LABELS` — because that is what `toolAction` does, and a tool
        // present in both resolves to the effect sentence. Comparing the tables
        // separately would miss exactly the collision that ordering creates.
        let sentence = |tool: &str| -> &str {
            effect_labels
                .iter()
                .chain(tool_labels.iter())
                .find(|(key, _)| key == tool)
                .map(|(_, value)| value.as_str())
                .expect("every gated tool is labelled — asserted directly above")
        };

        let mut by_sentence: std::collections::BTreeMap<&str, Vec<&str>> = Default::default();
        for tool in &gated {
            by_sentence.entry(sentence(tool)).or_default().push(tool);
        }
        let collisions: Vec<String> = by_sentence
            .iter()
            .filter(|(_, sharing)| sharing.len() > 1)
            .map(|(reads, sharing)| format!("{sharing:?} all read {reads:?}"))
            .collect();
        assert!(
            collisions.is_empty(),
            "two gated tools render the same sentence: {}. The Standing \
             permissions list (#374) puts no payload block under a row, so two \
             rows reading alike are two permissions an operator cannot choose \
             between — and on an approval card the payload only disambiguates \
             them if they happen to carry different arguments. Give each its own \
             words in {LANGUAGE_TS}",
            collisions.join("; ")
        );
    }

    // -----------------------------------------------------------------------
    // Issue #875: `shell`, classified by the command it was handed
    // -----------------------------------------------------------------------

    // Gated to match its callers. Every test below that grades a shell command
    // is `#[cfg(feature = "openhuman")]`, so without the feature they compile
    // away and this helper is left with none — `dead_code` under the default
    // lane's `-D warnings`, which is what turned the `Rust` job red.
    #[cfg(feature = "openhuman")]
    fn shell(command: &str) -> Consequence {
        consequence_of(SHELL, &json!({ SHELL_COMMAND_KEY: command }))
    }

    /// The complaint this issue is about: an agent looking at its own workspace
    /// paid an approval per command. These are the exact shapes an operator was
    /// approving on staging.
    #[test]
    #[cfg(feature = "openhuman")]
    fn a_read_of_the_agents_own_workspace_runs_unattended() {
        for command in [
            "grep -l -i \"resets\\|forgot\" session_raw/*.jsonl",
            "grep -c -i plus session_raw/*.jsonl",
            "find . -maxdepth 4 -type d",
            "cat notes.md",
            "ls -la",
            "wc -l src/main.rs",
        ] {
            let c = shell(command);
            assert_eq!(
                c.reach,
                Reach::Nothing,
                "`{command}` reads and changes nothing"
            );
            assert!(
                !c.reach.parks_under_supervision(),
                "`{command}` must not park under any acting tier"
            );
        }
    }

    /// Bases omitted from the vendor's name-only allowlist may run unattended
    /// only when their actual argv excludes every writing form we admit around.
    #[test]
    #[cfg(feature = "openhuman")]
    fn argv_sensitive_workspace_reads_run_without_admitting_writes() {
        for command in [
            "sed -n '860,915p' src/policy/consequence.rs",
            "sed 's/old/new/g' notes.txt",
            "sort names.txt",
            "sort -r names.txt",
            "awk '{ print $1 }' data.txt",
            "awk -F, '{ print $2 }' data.csv",
        ] {
            assert_eq!(
                shell(command).reach,
                Reach::Nothing,
                "`{command}` has a provably read-only argv"
            );
        }

        for command in [
            "sed -i 's/old/new/g' notes.txt",
            "sed -ni.bak 's/old/new/g' notes.txt",
            "sed --in-place=.bak 's/old/new/g' notes.txt",
            "sed -f transform.sed notes.txt",
            "sort -o sorted.txt names.txt",
            "sort -ruooutput.txt names.txt",
            "sort --output=sorted.txt names.txt",
            "sort --compress-program=gzip names.txt",
            "awk '{ print $1 > \"out.txt\" }' data.txt",
            "awk '{ print $1 }' data.txt > out.txt",
            "awk '{ print $1 | \"tee out.txt\" }' data.txt",
            "awk 'BEGIN { system(\"touch out.txt\") }' data.txt",
            "awk -f report.awk data.txt",
        ] {
            assert_eq!(
                shell(command).reach,
                Reach::Consequence,
                "`{command}` can write or execute and must park"
            );
        }
    }

    /// The argv exceptions remain behind #876's lexical containment boundary.
    #[test]
    #[cfg(feature = "openhuman")]
    fn argv_sensitive_reads_still_refuse_escapes_and_ambiguous_shell_syntax() {
        for command in [
            "sed -n '1p' /etc/passwd",
            "sort /etc/passwd",
            "awk '{ print $1 }' /etc/passwd",
            "sed -n '1p' ~/.ssh/config",
            "sort ~/secrets.txt",
            "awk '{ print $1 }' ~/.secrets",
            "sed -n '1p' ../secret.txt",
            "sort data/../../secret.txt",
            "awk '{ print $1 }' ../secret.txt",
            "sed -n \"$(cat program.sed)\" notes.txt",
            "sort \"$(cat filenames.txt)\"",
            "awk \"$(cat program.awk)\" data.txt",
            "sed -n '1p notes.txt",
            "sort $SORT_FLAGS names.txt",
            "awk '{ print $1 }' data.txt; rm data.txt",
        ] {
            assert_eq!(
                shell(command).reach,
                Reach::Consequence,
                "`{command}` is outside the lexical boundary or has ambiguous argv"
            );
        }
    }

    /// A command the vendored classifier grades `Read` — because it grades by
    /// command name, never by path — must still park when its arguments name a
    /// location outside the agent's own directory. Falsified against the
    /// pre-fix behaviour: before `shell_command_reaches_outside_cwd` existed,
    /// `shell_command_is_read` alone was sufficient and every one of these
    /// downgraded to `Reach::Nothing` — `cat`/`ls`/`grep`/`readlink` are all in
    /// the vendored `READ_ONLY_BASES` regardless of what they are pointed at.
    #[test]
    #[cfg(feature = "openhuman")]
    fn a_read_that_reaches_outside_the_workspace_still_parks() {
        for command in [
            "cat /etc/passwd",
            "cat ~/.ssh/id_rsa",
            "ls /root",
            "grep -r secret /etc",
            "readlink ~",
            "cat ../../secrets.env",
            "head --lines=5 /var/log/auth.log",
            "cat notes/../../../etc/passwd",
        ] {
            let c = shell(command);
            assert_eq!(
                c.reach,
                Reach::Consequence,
                "`{command}` reaches outside the workspace and must still park"
            );
            assert!(
                c.parks_under_auto(),
                "`{command}` must still park under auto"
            );
        }
    }

    /// The lexical backstop alone, independent of the classifier — pins the
    /// exact set of shapes it does and does not flag. No `openhuman` feature
    /// needed: this is pure string logic with no classifier dependency.
    #[test]
    fn shell_command_reaches_outside_cwd_flags_the_realistic_escapes() {
        for command in [
            "cat /etc/passwd",
            "ls ~/.ssh",
            "cat ../secret.env",
            "cat notes/../../etc/passwd",
            "head --lines=5 /var/log/auth.log",
            "cat \"/etc/passwd\"",
        ] {
            assert!(
                shell_command_reaches_outside_cwd(command),
                "`{command}` should be flagged as reaching outside the cwd"
            );
        }

        for command in [
            "cat notes.md",
            "grep -l foo session_raw/*.jsonl",
            "find . -maxdepth 4 -type d",
            "ls -la",
            "wc -l src/main.rs",
        ] {
            assert!(
                !shell_command_reaches_outside_cwd(command),
                "`{command}` stays inside the cwd and should not be flagged"
            );
        }
    }

    /// Everything that is not provably a read keeps exactly the verdict it had
    /// before this issue: it parks, and it can hold no standing grant.
    #[test]
    #[cfg(feature = "openhuman")]
    fn anything_that_acts_still_parks() {
        for command in [
            "rm -rf /",
            "curl https://example.com",
            "npm install -g something",
            "echo hi > file.txt",
            "git push origin main",
            "chmod 777 /etc/passwd",
        ] {
            let c = shell(command);
            assert_eq!(c.reach, Reach::Consequence, "`{command}` acts");
            assert_eq!(
                c.standing,
                Standing::PerCall,
                "`{command}` may hold no standing grant"
            );
            assert!(
                c.parks_under_auto(),
                "`{command}` must still park under auto"
            );
        }
    }

    // ── git_operations, graded by its `operation` (issue #877) ─────────────

    fn git(operation: &str) -> Consequence {
        consequence_of(GIT_OPERATIONS, &json!({ GIT_OPERATION_KEY: operation }))
    }

    /// Orienting in your own workspace should not cost an operator anything.
    #[test]
    fn a_git_read_operation_does_not_park() {
        for operation in GIT_READ_ONLY_OPERATIONS {
            let c = git(operation);
            assert_eq!(
                c.reach,
                Reach::Nothing,
                "`git {operation}` only reads the repository"
            );
            assert!(
                !c.parks_under_auto(),
                "`git {operation}` must not interrupt anybody"
            );
        }
    }

    /// The writes upstream names still park. Without this the downgrade above
    /// would pass against a build that stopped gating everything.
    #[test]
    fn a_git_write_operation_still_parks() {
        for operation in ["commit", "add", "checkout", "stash", "reset", "revert"] {
            let c = git(operation);
            assert_eq!(c.reach, Reach::Consequence, "`git {operation}` acts");
            assert!(
                c.parks_under_auto(),
                "`git {operation}` must still park under auto"
            );
        }
    }

    /// **The fail-closed requirement.** An operation this classifier does not
    /// recognise must still ask.
    ///
    /// The first six are real git subcommands in **neither** upstream list —
    /// `requires_write_access` does not name them and `is_read_only` does not
    /// either — so they are genuinely unclassified rather than merely absent
    /// from a list somebody forgot to extend. `push` is the one that matters
    /// most: it reaches a configured remote, which is an address this layer
    /// never sees. The last two are a typo and an invented name, which is what
    /// a model produces on a bad day.
    ///
    /// This passing is the whole safety argument for the downgrade: membership
    /// is affirmative, so the failure mode of an unknown operation is an extra
    /// approval, never a silent act.
    #[test]
    fn an_unrecognised_git_operation_still_parks() {
        for operation in [
            "push",
            "pull",
            "fetch",
            "merge",
            "rebase",
            "clone",
            "stauts",
            "frobnicate",
        ] {
            let c = git(operation);
            assert_eq!(
                c.reach,
                Reach::Consequence,
                "`git {operation}` is not provably a read, so it must ask"
            );
            assert!(
                c.parks_under_auto(),
                "`git {operation}` must park under auto"
            );
        }
    }

    /// An argument that cannot be read gates. The tool's schema marks
    /// `operation` required, so each of these is a call that could not have run
    /// — guessing at one would be inventing a verdict for a call that never
    /// happened.
    #[test]
    fn a_git_call_with_no_readable_operation_parks() {
        for args in [
            json!({}),
            json!({ GIT_OPERATION_KEY: null }),
            json!({ GIT_OPERATION_KEY: 7 }),
            json!({ GIT_OPERATION_KEY: ["status"] }),
            json!({ "op": "status" }),
        ] {
            let c = consequence_of(GIT_OPERATIONS, &args);
            assert_eq!(
                c.reach,
                Reach::Consequence,
                "unreadable args must park: {args}"
            );
        }
    }

    /// Case matters, matching upstream's `matches!`. `STATUS` is not `status`,
    /// and a classifier that normalised case here would be answering a question
    /// upstream does not ask.
    #[test]
    fn git_operation_matching_is_case_sensitive() {
        for operation in ["STATUS", "Status", "LOG"] {
            assert_eq!(
                git(operation).reach,
                Reach::Consequence,
                "`{operation}` is not the operation upstream classifies"
            );
        }
    }

    /// **The oracle.** [`GIT_READ_ONLY_OPERATIONS`] is a copy of a vendored
    /// list, and a copy that can drift silently is exactly what issue #877
    /// warns against. This drives the vendored judgement directly, so upstream
    /// reclassifying any of these fails the build here rather than quietly
    /// widening what runs unattended.
    ///
    /// It asserts the safety-relevant direction: **none of the operations this
    /// crate downgrades is a write upstream**. The converse is not assertable —
    /// `is_read_only` is a private inherent method — but it is also not the
    /// dangerous direction: an operation upstream calls read-only that we
    /// nonetheless gate costs an approval, while the reverse would run a write
    /// unattended.
    ///
    /// `SecurityPolicy::default()` is `AutonomyLevel::Supervised`, where
    /// `gate_decision(Write)` is `Prompt` — so the tier half of
    /// `external_effect_with_args`'s conjunction is `true` and the expression
    /// reduces to `requires_write_access(operation)` alone. That is the only
    /// configuration in which this hook answers the question this crate is
    /// asking, which is why the gate itself must not call it (see
    /// [`git_operations_consequence`]).
    #[test]
    #[cfg(feature = "openhuman")]
    fn the_read_only_set_matches_the_vendored_classifier() {
        use openhuman_core::openhuman::security::SecurityPolicy;
        use openhuman_core::openhuman::tools::{GitOperationsTool, Tool};

        let policy = std::sync::Arc::new(SecurityPolicy::default());
        let tool = GitOperationsTool::new(policy, std::path::PathBuf::from("."));

        for operation in GIT_READ_ONLY_OPERATIONS {
            assert!(
                !tool.external_effect_with_args(&json!({ GIT_OPERATION_KEY: operation })),
                "upstream now treats `git {operation}` as a write — this crate is downgrading \
                 something that acts. Remove it from GIT_READ_ONLY_OPERATIONS."
            );
        }

        // And the pairing that proves the oracle is live rather than vacuous: a
        // known write must come back `true` through the same call.
        assert!(
            tool.external_effect_with_args(&json!({ GIT_OPERATION_KEY: "commit" })),
            "the oracle answered `false` for a commit, so it is not testing anything"
        );
    }

    /// The classifier takes the maximum across segments, so a read cannot carry
    /// an act through on its coat-tails. This is the property that makes
    /// downgrading reads safe at all.
    #[test]
    #[cfg(feature = "openhuman")]
    fn a_read_chained_to_an_act_is_an_act() {
        for command in [
            "grep -r foo . && rm -rf /tmp/x",
            "ls; curl https://example.com",
            "cat a.txt | tee b.txt",
            "find . -type f > listing.txt",
        ] {
            assert_eq!(
                shell(command).reach,
                Reach::Consequence,
                "`{command}` contains an act and must park"
            );
        }
    }

    /// The model's own label may raise the requirement and never lower it.
    #[test]
    #[cfg(feature = "openhuman")]
    fn a_declared_category_escalates_only() {
        // A read the model calls destructive parks…
        let escalated = consequence_of(
            SHELL,
            &json!({ SHELL_COMMAND_KEY: "ls -la", SHELL_CATEGORY_KEY: "destructive" }),
        );
        assert_eq!(escalated.reach, Reach::Consequence);

        // …and an act the model calls a read does not stop parking.
        let attempted_downgrade = consequence_of(
            SHELL,
            &json!({ SHELL_COMMAND_KEY: "rm -rf /", SHELL_CATEGORY_KEY: "read" }),
        );
        assert_eq!(attempted_downgrade.reach, Reach::Consequence);
    }

    /// A call this cannot read is gated. The tool's schema requires `command`,
    /// so every one of these is a call that could not have run — and none of
    /// them is a reason to guess.
    #[test]
    fn an_unreadable_shell_call_is_gated() {
        for args in [
            json!({}),
            json!({ SHELL_COMMAND_KEY: 7 }),
            json!({ SHELL_COMMAND_KEY: null }),
            json!(null),
            json!("ls"),
        ] {
            let c = consequence_of(SHELL, &args);
            assert_eq!(c.reach, Reach::Consequence, "{args}");
            assert!(c.parks_under_auto(), "{args}");
        }
    }

    /// The name-level declaration is untouched: every reader that asks about
    /// `shell` without arguments — the permissions list, the console labels,
    /// the coverage test — still sees the gated answer.
    #[test]
    fn the_declaration_still_reads_as_gated_without_arguments() {
        assert_eq!(c(SHELL).reach, Reach::Consequence);
    }

    /// Without the harness feature there is no classifier, and the fallback
    /// answers "act" for everything. Nothing pinned that: the gated-call test
    /// above passes only malformed arguments, which return before
    /// `shell_command_is_read` is ever reached, so the fallback could regress to
    /// permissive and every default-feature lane would stay green. A command
    /// that IS a read under the classifier is the case that separates them.
    #[test]
    #[cfg(not(feature = "openhuman"))]
    fn a_read_command_still_parks_when_no_classifier_is_linked_in() {
        let c = consequence_of(SHELL, &json!({ SHELL_COMMAND_KEY: "ls -la" }));
        assert_eq!(c.reach, Reach::Consequence);
        assert!(c.parks_under_auto());
    }

    // ── MCP bridge calls, graded against a per-server read declaration (#1124) ──

    /// A `mcp_call_tool` call as the policy layer sees it.
    fn mcp_call(server: &str, tool: &str) -> serde_json::Value {
        json!({
            MCP_CALL_SERVER_KEY: server,
            MCP_CALL_TOOL_KEY: tool,
            "arguments": {},
        })
    }

    /// A `mcp_registry_tool_call` call — different argument keys, same shape.
    fn registry_call(server_id: &str, tool_name: &str) -> serde_json::Value {
        json!({
            MCP_REGISTRY_SERVER_KEY: server_id,
            MCP_REGISTRY_TOOL_KEY: tool_name,
            "arguments": {},
        })
    }

    /// **Acceptance criterion 1, for both tools.** A call to a server-declared
    /// read-only remote tool does not park under `auto`; every other combination
    /// still parks.
    ///
    /// This is the classifier's OWN test (criterion 4): reverting
    /// [`mcp_call_reach`] to return its base for the declared pair — the whole of
    /// the downgrade — makes the first two assertions fail, because the declared
    /// read would park again.
    #[test]
    fn a_declared_read_only_remote_tool_does_not_park_but_everything_else_does() {
        let reads = McpReadSet::from_pairs([
            ("jira".to_string(), "get_issue".to_string()),
            ("registry-42".to_string(), "list_rows".to_string()),
        ]);

        // The declared read on each tool downgrades and stops parking.
        let call_read = mcp_call_reach(MCP_CALL_TOOL, &mcp_call("jira", "get_issue"), &reads);
        assert_eq!(call_read.reach, Reach::ExternalRead);
        assert!(
            !call_read.parks_under_auto(),
            "a server-declared read must not park under auto"
        );
        let registry_read = mcp_call_reach(
            MCP_REGISTRY_TOOL_CALL,
            &registry_call("registry-42", "list_rows"),
            &reads,
        );
        assert_eq!(registry_read.reach, Reach::ExternalRead);
        assert!(!registry_read.parks_under_auto());

        // Every other combination still parks: a write on the same declared
        // server, a read on an undeclared server, and the same declared tool name
        // on the WRONG tool of the pair (server declared, tool not).
        for (tool, args) in [
            (MCP_CALL_TOOL, mcp_call("jira", "create_issue")),
            (MCP_CALL_TOOL, mcp_call("confluence", "get_issue")),
            (MCP_CALL_TOOL, mcp_call("jira", "list_rows")),
            (
                MCP_REGISTRY_TOOL_CALL,
                registry_call("registry-42", "write_row"),
            ),
            (
                MCP_REGISTRY_TOOL_CALL,
                registry_call("registry-99", "list_rows"),
            ),
            // The keys are not interchangeable across the two tools: a
            // registry-shaped payload under `mcp_call_tool` reads no `server`.
            (MCP_CALL_TOOL, registry_call("jira", "get_issue")),
        ] {
            let verdict = mcp_call_reach(tool, &args, &reads);
            assert_eq!(
                verdict.reach,
                Reach::Consequence,
                "`{tool}` {args} is not an affirmatively-declared read and must park"
            );
            assert!(
                verdict.parks_under_auto(),
                "`{tool}` {args} must park under auto"
            );
        }
    }

    /// The fail-closed base: with no declaration, every bridge call parks — the
    /// verdict both tools carried before this issue, and the answer for every
    /// non-harness construction site whose policy sets no read declaration.
    #[test]
    fn with_no_declaration_every_bridge_call_gates() {
        let empty = McpReadSet::default();
        assert!(empty.is_empty());
        for (tool, args) in [
            (MCP_CALL_TOOL, mcp_call("jira", "get_issue")),
            (MCP_REGISTRY_TOOL_CALL, registry_call("r", "get_issue")),
        ] {
            let verdict = mcp_call_reach(tool, &args, &empty);
            assert_eq!(verdict.reach, Reach::Consequence);
            assert_eq!(verdict.standing, Standing::PerCall);
            assert!(verdict.parks_under_auto());
        }
    }

    /// A downgraded read is `ExternalRead`, not `Nothing`: it reaches a third
    /// party's server with the company's credential, so a `readonly` desk still
    /// denies it and it is never billed — the Composio-read precedent (#559).
    #[test]
    fn a_downgraded_read_is_denied_under_readonly_and_is_not_a_spend() {
        let reads = McpReadSet::from_pairs([("jira".to_string(), "get_issue".to_string())]);
        let verdict = mcp_call_reach(MCP_CALL_TOOL, &mcp_call("jira", "get_issue"), &reads);
        assert_eq!(verdict.reach, Reach::ExternalRead);
        assert!(
            verdict.reach.denied_under_readonly(),
            "a read of a counterparty's account is exactly what readonly refuses"
        );
        assert!(!verdict.reach.costs_money(), "a read is not billed");
        assert!(
            !verdict.reach.parks_under_supervision(),
            "supervised runs it — nothing changes and nothing is spent"
        );
        assert_eq!(verdict.standing, Standing::PerCall);
    }

    /// A call this cannot read gates, whichever key is missing or mistyped. The
    /// tools' schemas mark both required, so each of these is a call that could
    /// not have run — the same fail-closed rule the other argument graders keep.
    #[test]
    fn an_unreadable_bridge_call_gates_even_with_a_matching_declaration() {
        let reads = McpReadSet::from_pairs([
            ("jira".to_string(), "get_issue".to_string()),
            ("r".to_string(), "get_issue".to_string()),
        ]);
        let unreadable_call = [
            json!({ MCP_CALL_TOOL_KEY: "get_issue", "arguments": {} }), // no server
            json!({ MCP_CALL_SERVER_KEY: "jira", "arguments": {} }),    // no tool
            json!({ MCP_CALL_SERVER_KEY: 7, MCP_CALL_TOOL_KEY: "get_issue" }), // non-string
            json!({ MCP_CALL_SERVER_KEY: "jira", MCP_CALL_TOOL_KEY: null }),
            json!(null),
            json!("jira"),
        ];
        for args in unreadable_call {
            let verdict = mcp_call_reach(MCP_CALL_TOOL, &args, &reads);
            assert_eq!(verdict.reach, Reach::Consequence, "unreadable: {args}");
            assert!(verdict.parks_under_auto(), "unreadable: {args}");
        }
        // …and the registry twin, under its own keys.
        for args in [
            json!({ MCP_REGISTRY_TOOL_KEY: "get_issue", "arguments": {} }),
            json!({ MCP_REGISTRY_SERVER_KEY: "r", "arguments": {} }),
            json!({ MCP_REGISTRY_SERVER_KEY: "r", MCP_REGISTRY_TOOL_KEY: 7 }),
        ] {
            let verdict = mcp_call_reach(MCP_REGISTRY_TOOL_CALL, &args, &reads);
            assert_eq!(
                verdict.reach,
                Reach::Consequence,
                "unreadable registry: {args}"
            );
        }
    }

    /// The tool name is matched case-insensitively, the way every other arm of
    /// the gate reads it — the argument keys, and the bridge-tool predicate.
    #[test]
    fn the_bridge_tool_name_is_matched_case_insensitively() {
        let reads = McpReadSet::from_pairs([("jira".to_string(), "get_issue".to_string())]);
        assert!(is_mcp_bridge_tool("MCP_CALL_TOOL"));
        assert!(is_mcp_bridge_tool("Mcp_Registry_Tool_Call"));
        assert!(!is_mcp_bridge_tool("mcp_list_tools"));
        let verdict = mcp_call_reach("MCP_CALL_TOOL", &mcp_call("jira", "get_issue"), &reads);
        assert_eq!(verdict.reach, Reach::ExternalRead);
    }

    /// The plain `consequence_of` — which the roster, the coverage test and every
    /// company-blind caller read — still sees the gated verdict for both bridge
    /// tools. The downgrade lives only where the declaration does, on the policy.
    #[test]
    fn consequence_of_reads_both_bridge_tools_as_gated() {
        for tool in [MCP_CALL_TOOL, MCP_REGISTRY_TOOL_CALL] {
            let verdict = consequence_of(tool, &mcp_call("jira", "get_issue"));
            assert_eq!(verdict.reach, Reach::Consequence, "`{tool}`");
            assert_eq!(verdict.standing, Standing::PerCall, "`{tool}`");
            assert!(verdict.parks_under_auto(), "`{tool}`");
        }
    }

    /// **Acceptance criterion 3.** Both bridge tools sit on the argument-graded
    /// side of the partition, so the roster and the table stay disjoint and
    /// `declared_tools` enumerates each exactly once. This is a direct probe of
    /// the same facts `the_roster_and_the_table_partition_the_known_tool_names`
    /// enforces over the whole set, named here so a reader of this issue's change
    /// sees the criterion asserted.
    #[test]
    fn both_bridge_tools_are_argument_graded_and_enumerated_once() {
        for tool in [MCP_CALL_TOOL, MCP_REGISTRY_TOOL_CALL] {
            assert!(
                argument_grader(tool).is_some(),
                "`{tool}` must be dispatched from its arguments"
            );
            assert_eq!(
                declared_tools().filter(|name| *name == tool).count(),
                1,
                "`{tool}` holds both a roster entry and a DECLARED row and must be enumerated once"
            );
        }
    }
}
