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
//! must not inherit a week-long capability by omission. Likewise an unrecognised
//! Composio action slug is a **send**, not a read.

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
    // Both are confined to the agent's own `Agents/<self>/` folder, which is
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
    d("mcp_call_tool", EffectGroup::Other, Reach::Consequence),
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
    d(
        "mcp_registry_tool_call",
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
    // ---- Bound repositories -------------------------------------------------
    // Issue #245's agent half. The classification is re-derived rather than
    // borrowed from the nearest-looking neighbour, because both names read like
    // reads and neither is one in the sense this table means.
    //
    // `Consequence`, not `Nothing`, for two reasons that hold independently:
    //
    //  1. **Both pull third-party-authored content into the agent's context.**
    //     A repository's source and a pull request's diff are written by people
    //     outside this company, and the agent is about to reason over them. That
    //     is the same shape as `web_fetch` — which is `Consequence` for exactly
    //     this reason — not the shape of reading the company's own notes.
    //  2. **Both reach the forge host-side under the operator's credential.**
    //     `repo_checkout` refreshes the mirror over the network before it
    //     clones; `repo_pr` is a GitHub API call. An agent deciding when a
    //     company's credential is used is a decision an operator would want to
    //     have seen.
    //
    // `repo_checkout` additionally materializes thousands of files into a
    // sandbox the same agent may hold `shell` over, which is why it is denied
    // under `readonly` — a tier whose contract is that nothing changes cannot
    // admit a tool whose whole purpose is to write a tree.
    //
    // `PerCall` for both, and this is the part a future edit is most likely to
    // want to loosen: a standing grant here would be a week of unattended
    // "check out anything bound, whenever you like", which is precisely the
    // permission the `Standing` field refuses to describe. `EffectGroup::Other`
    // because there is no consequence word for it — the label and the
    // permission are separate answers (issue #444).
    d("repo_checkout", EffectGroup::Other, Reach::Consequence),
    d("repo_pr", EffectGroup::Other, Reach::Consequence),
    // `repo_publish` (issue #735) is classified by what the CALL does, which is
    // deliberately NOT what its approval settles. The call stages the agent's
    // committed work onto a host-side `oc/<company>/<task>` ref in the mirror and
    // records an operator approval. It reaches no counterparty, spends nothing,
    // and the stage is reversible and never leaves the host — so `Nothing` at the
    // tool layer. The irreversible push to the real remote is a separate native
    // effect (`repo.publish`, `EffectGroup::Publish`) that the runtime performs
    // ONLY on the operator's approval, so *that* effect is where the consequence
    // and its gate live — see the `repo.publish` arm of `perform_effect`.
    //
    // `Nothing`, not `Consequence`, is load-bearing rather than a downgrade: a
    // `Consequence` call PARKS under `supervised`, and a parked call whose
    // `execute` never ran would have nothing to stage — the checkout it stages
    // from is deleted at turn end. So the call must run in every mode, and it
    // does no external harm in any of them: no agent-driven change reaches a
    // remote without an operator approving the push, which is the property
    // `readonly` actually promises, kept here by the approval rather than by
    // refusing a harmless local stage. `EffectGroup::Publish` is the label the
    // operator's approval card carries; `PerCall` because a standing "publish
    // whenever" is exactly the grant the `Standing` field refuses to describe,
    // and every push already parks as its own approval regardless.
    d("repo_publish", EffectGroup::Publish, Reach::Nothing),
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

/// Every tool name [`DECLARED`] classifies, for the coverage test.
pub fn declared_tools() -> impl Iterator<Item = &'static str> {
    DECLARED
        .iter()
        .map(|d| d.tool)
        .chain(std::iter::once(COMPOSIO_EXECUTE))
}

/// What this tool call can reach, and what an operator may do about it.
///
/// `args` are consulted, not decoration: `composio_execute` carries every
/// Composio action under one name, so classifying it from the name alone
/// collapsed a repository read and an outgoing email into the same verdict —
/// and the cautious answer had to win for both (issue #441).
pub fn consequence_of(tool: &str, args: &serde_json::Value) -> Consequence {
    let name = tool.to_ascii_lowercase();
    if name == COMPOSIO_EXECUTE {
        return composio_execute_consequence(args);
    }
    // Issue #673: the second argument-classified tool. Its declaration below
    // stays as the shape every reader of `DECLARED` expects — this only decides
    // whether the operator gets a host to consent to.
    if name == WEB_FETCH {
        return web_fetch_consequence(args);
    }
    // Issue #875: the third. `shell` is the tool an agent reaches for to look
    // at its own workspace, and classifying the NAME made a `grep` cost an
    // operator the same interruption as `rm -rf /`.
    if name == SHELL {
        return shell_consequence(args);
    }
    // Issue #877: the fourth. `git_operations` is how an agent orients in its own
    // workspace, and classifying the NAME charged a `git status` the same
    // interruption as a `git push` to a configured remote.
    if name == GIT_OPERATIONS {
        return git_operations_consequence(args);
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
/// ## Anything the catalogue does not name is a send
///
/// Deliberately **not** upstream's `classify_unknown`, whose fallback for an
/// unrecognised slug is `Read`. That is the convenient verdict, and this is the
/// place where the cautious one has to win: an action nobody has classified
/// might do anything, so it parks and it cannot be granted standing. Same for a
/// slug whose toolkit has no catalogue, a missing or non-string `tool`
/// argument, and — in a build without the harness compiled in — every slug.
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
    if composio_action_is_read(slug) {
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
        send
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
/// seam `composio_action_is_read` straddles, answered the same cautious way.
#[cfg(not(feature = "openhuman"))]
fn composio_toolkit_of(_slug: &str) -> Option<String> {
    None
}

/// Is this Composio action slug a read, according to the provider's own
/// curated catalogue? Unknown is **not** a read.
#[cfg(feature = "openhuman")]
fn composio_action_is_read(slug: &str) -> bool {
    use openhuman_core::openhuman::memory::sync::composio::providers::{
        ToolScope, catalog_for_toolkit, find_curated, toolkit_from_slug,
    };
    let Some(toolkit) = toolkit_from_slug(slug) else {
        return false;
    };
    let Some(catalog) = catalog_for_toolkit(&toolkit) else {
        return false;
    };
    matches!(
        find_curated(catalog, slug).map(|entry| entry.scope),
        Some(ToolScope::Read)
    )
}

/// Without the harness feature the curated catalogue is not linked in, and no
/// `composio_execute` call can be made either — only replayed from a journal
/// line an openhuman build wrote. Cautious is the only honest answer.
#[cfg(not(feature = "openhuman"))]
fn composio_action_is_read(_slug: &str) -> bool {
    false
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
            // Issue #245: both reach a forge under the company's credential and
            // pull third-party-authored content into the agent's context, and
            // one of them writes a tree.
            "repo_checkout",
            "repo_pr",
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
            "repo_checkout",
            "repo_pr",
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

    /// The cautious direction, four ways. An action the catalogue does not
    /// name, a toolkit it has never heard of, a missing slug and a slug of the
    /// wrong type all read as sends.
    #[test]
    fn an_unrecognised_composio_action_is_a_send() {
        for args in [
            json!({ "tool": "GITHUB_INVENT_A_NEW_VERB" }),
            json!({ "tool": "NOTAREALTOOLKIT_LIST_THINGS" }),
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
        for slug in ["GITHUB_LIST_PULL_REQUESTS", "GMAIL_SEND_EMAIL"] {
            let verdict = consequence_of(COMPOSIO_EXECUTE, &json!({ "tool": slug }));
            assert_eq!(verdict.group, EffectGroup::Send, "{slug}");
            assert_eq!(verdict.standing, Standing::PerCall, "{slug}");
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
        assert!(!composio_action_is_read("GITHUB_INVENT_A_NEW_VERB"));
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
        assert_eq!(all.len(), DECLARED.len() + 1);
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
            tool: COMPOSIO_EXECUTE.to_string(),
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
    /// grant refuses — so an unplaceable slug parks rather than riding somebody
    /// else's permission.
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
}
