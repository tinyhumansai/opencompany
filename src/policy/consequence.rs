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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Standing {
    /// An operator may grant this to a teammate until a deadline — **and**,
    /// since issue #560, may run unattended under the `auto` tier. See the note
    /// on [`Standing`] before loosening a tool to this.
    Grantable,
    /// Every call is its own decision.
    PerCall,
}

impl Standing {
    /// May this be granted standing?
    pub fn is_grantable(self) -> bool {
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
    pub fn parks_under_auto(self) -> bool {
        self.reach.parks_under_supervision() && !self.standing.is_grantable()
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
    d("add_agent", EffectGroup::Other, Reach::Nothing),
    d("create_workflow", EffectGroup::Other, Reach::Nothing),
    d("assign_task", EffectGroup::Other, Reach::Nothing),
    d("review_task", EffectGroup::Other, Reach::Nothing),
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
    d("web_fetch", EffectGroup::Other, Reach::Consequence),
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
    d("publish_artifact", EffectGroup::Publish, Reach::Consequence),
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
    // ---- Skills / workflow catalogue ---------------------------------------
    // The three OpenHuman skill *read* tools, scoped to this agent's own
    // materialized skill tree under its workspace. All local, all reads.
    //
    // `describe_workflow` PARKED before this table existed, for the same
    // reason `file_read` did and with nobody reporting either: the read-only
    // rule matched a name *prefix* and "describe" is not one of the words. The
    // persona hands an agent all three in one sentence — "use `list_workflows`
    // to enumerate them, `describe_workflow` to inspect one" — so two ran and
    // the middle one interrupted an operator.
    d("list_workflows", EffectGroup::Other, Reach::Nothing),
    d("describe_workflow", EffectGroup::Other, Reach::Nothing),
    d("read_workflow_resource", EffectGroup::Other, Reach::Nothing),
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

/// Every [`Reach::Consequence`] tool in [`DECLARED`], sorted and deduplicated.
///
/// This is exactly the set an operator can be shown an approval card for: under
/// `supervised` — the default mode — `Consequence` is the reach that parks, so
/// every name here reaches a human who has to decide about it. The console must
/// therefore have plain-language words for all of them, and
/// `frontend/src/lib/language.ts` is where those words live.
///
/// That is a seam with no compiler across it: the declarations are Rust, the
/// labels are TypeScript, and nothing in either build has ever compared them.
/// `workspace_create` sat showing the generic "Use one of its tools" from issue
/// #551 until #706 without a single check failing. This function is the Rust
/// half of closing that gap — it generates
/// `frontend/src/lib/gated-tools.generated.ts`, which a frontend test reads to
/// assert every name in it resolves to a real label.
///
/// `composio_execute` is deliberately absent, as it is from the table itself:
/// its reach is read from the action slug in its arguments rather than declared
/// statically, so there is no single answer to snapshot. The console labels it
/// through `EFFECT_LABELS` instead.
pub fn consequence_tools() -> Vec<&'static str> {
    let mut tools: Vec<&'static str> = DECLARED
        .iter()
        .filter(|d| d.reach == Reach::Consequence)
        .map(|d| d.tool)
        .collect();
    tools.sort_unstable();
    tools.dedup();
    tools
}

/// The generated TypeScript module that carries [`consequence_tools`] to the
/// console, byte for byte as it must appear on disk.
///
/// Kept beside the list rather than in the test so the snapshot test and the
/// regenerator cannot disagree about the format — the same reason
/// `sdl_snapshot_matches` and `regenerate_sdl_snapshot` both call `sdl()`.
pub fn generated_gated_tools_ts() -> String {
    let mut out = String::from(
        "// @generated by `cargo test -- --ignored regenerate_gated_tools` — do not edit.\n\
         //\n\
         // Every tool the runtime classifies as `Reach::Consequence` in\n\
         // `src/policy/consequence.rs`, which is exactly the set that parks for\n\
         // approval under the default `supervised` mode. `gated-tool-labels.test.ts`\n\
         // asserts each one resolves to plain language in `language.ts`, so a tool\n\
         // added to the Rust table cannot reach an operator unnamed (issue #706).\n\
         export const GATED_TOOLS = [\n",
    );
    for tool in consequence_tools() {
        out.push_str("  \"");
        out.push_str(tool);
        out.push_str("\",\n");
    }
    out.push_str("] as const;\n");
    out
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
/// [`StandingGrant::admits_scope`]: crate::runtime::grants::StandingGrant::admits_scope
pub fn standing_scope_of(tool: &str, args: &serde_json::Value) -> Option<String> {
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
        ];

        let mut moved: Vec<&str> = declared_tools()
            .filter(|tool| {
                let verdict = c(tool);
                verdict.reach.parks_under_supervision() && !verdict.parks_under_auto()
            })
            .collect();
        moved.sort_unstable();
        assert_eq!(
            moved, MOVED_BY_AUTO,
            "a tool crossed the `auto` line. If that is intended, say so here — \
             `Standing::Grantable` now also means 'runs unattended for every agent \
             while the company sits in auto', which is wider than the standing \
             grant the field is named for"
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
            "publish_artifact",
            "media_generate_image",
            "media_generate_video",
            "mcp_call_tool",
            "run_workflow",
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
        use crate::harness::{orchestrator, publish, search, workspace_tools};
        for name in [
            orchestrator::QUERY_COMPANY_TOOL,
            orchestrator::SPAWN_TASK_TOOL,
            orchestrator::DELEGATE_TO_DESK_TOOL,
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

    /// The Rust half of the label seam (issue #706).
    ///
    /// The console's plain-language labels are TypeScript and this table is
    /// Rust, so nothing in either build compares them: `workspace_create` was
    /// showing an operator the generic "Use one of its tools" from issue #551
    /// until #706, and no test anywhere went red. This freezes the list the
    /// console must be able to name; `gated-tool-labels.test.ts` is the half
    /// that checks the names exist.
    ///
    /// Regenerate with
    /// `cargo test -- --ignored regenerate_gated_tools` after changing a
    /// declaration's [`Reach`].
    #[test]
    fn gated_tool_snapshot_matches() {
        let expected = include_str!("../../frontend/src/lib/gated-tools.generated.ts");
        let actual = generated_gated_tools_ts();
        assert_eq!(
            actual, expected,
            "the `Reach::Consequence` set drifted from \
             frontend/src/lib/gated-tools.generated.ts; regenerate with \
             `cargo test -- --ignored regenerate_gated_tools`"
        );
    }

    /// Fails **closed**, which is the whole reason this is a test and not a
    /// shell script over the table's source text.
    ///
    /// A check whose own extraction silently returns nothing reports success
    /// having compared nothing — the exact shape of the defect #706 is about.
    /// `consequence_tools` reads the typed table rather than parsing it, so it
    /// cannot return an empty list through a formatting change; this asserts
    /// that anyway, so a refactor that broke it would be loud rather than
    /// green.
    #[test]
    fn the_gated_tool_list_is_never_silently_empty() {
        let tools = consequence_tools();
        assert!(
            tools.len() > 10,
            "only {} tools classified as `Reach::Consequence` — the extraction \
             is broken, not the table: {tools:?}",
            tools.len()
        );
        // Named anchors, so a table-wide reclassification cannot leave this
        // passing on an unrelated remainder.
        for anchor in ["publish_artifact", "run_workflow", "shell"] {
            assert!(
                tools.contains(&anchor),
                "`{anchor}` parks for approval and must be in the gated list"
            );
        }
    }

    #[test]
    #[ignore = "writes the gated-tool snapshot; run explicitly after a Reach change"]
    fn regenerate_gated_tools() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("frontend/src/lib/gated-tools.generated.ts");
        std::fs::write(&path, generated_gated_tools_ts()).unwrap();
    }
}
