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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reach {
    /// Nothing changes and nothing is spent. Runs in every mode, `readonly`
    /// included. May still read over the network — see the note above.
    Nothing,
    /// Nothing changes, but the call is billed. Runs under `supervised`,
    /// denied under `readonly`.
    Money,
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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Standing {
    /// An operator may grant this to a teammate until a deadline.
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
const COMPOSIO_ACTION_KEY: &str = "tool";

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
    // ---- The agent's own sandboxed workspace: reads ------------------------
    // All four are pure reads inside the workspace the agent is pinned to.
    // `file_read`, `glob`, `grep` and `image_info` PARKED before this table
    // existed — not by anyone's decision, but because the read-only-prefix
    // heuristic keys on the *start* of the name and none of them begins with
    // one. `list`, `read_workspace_state` and `memory_recall` happened to.
    d("file_read", EffectGroup::Other, Reach::Nothing),
    d("glob", EffectGroup::Other, Reach::Nothing),
    d("grep", EffectGroup::Other, Reach::Nothing),
    d("list", EffectGroup::Other, Reach::Nothing),
    d("read_workspace_state", EffectGroup::Other, Reach::Nothing),
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
    d("http_request", EffectGroup::Other, Reach::Consequence),
    d("curl", EffectGroup::Other, Reach::Consequence),
    d("web_fetch", EffectGroup::Other, Reach::Consequence),
    // ---- The company workspace: operator-owned guidance --------------------
    // Reads are free (issue #237). `workspace_write` overwrites guidance the
    // operator wrote, which is why `is_external_effect` has always refused to
    // exempt it — and why it is now also refused a standing grant. That
    // contradiction (park every time / grant for a week) is issue #444's
    // headline, resolved in the direction the parking side already argued.
    d("workspace_list", EffectGroup::Other, Reach::Nothing),
    d("workspace_read", EffectGroup::Other, Reach::Nothing),
    d("workspace_write", EffectGroup::Other, Reach::Consequence),
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
fn composio_execute_consequence(args: &serde_json::Value) -> Consequence {
    let send = Consequence {
        group: EffectGroup::Send,
        reach: Reach::Consequence,
        standing: Standing::PerCall,
    };
    let Some(slug) = args.get(COMPOSIO_ACTION_KEY).and_then(|v| v.as_str()) else {
        return send;
    };
    if composio_action_is_read(slug) {
        // A read still reaches a third-party account, so `readonly` denies it
        // and `supervised` parks it the first time. What changes is that the
        // operator now has something to say other than yes-again: the card
        // offers a standing scope, and the reads stop asking for its duration.
        Consequence {
            group: EffectGroup::Other,
            reach: Reach::Consequence,
            standing: Standing::Grantable,
        }
    } else {
        send
    }
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
    let slug = args.get(COMPOSIO_ACTION_KEY).and_then(|v| v.as_str())?;
    composio_toolkit_of(slug)
}

/// The catalogued toolkit an action slug belongs to, or `None` when the
/// catalogue has never heard of it.
#[cfg(feature = "openhuman")]
fn composio_toolkit_of(slug: &str) -> Option<String> {
    use openhuman_core::openhuman::memory_sync::composio::providers::{
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
    use openhuman_core::openhuman::memory_sync::composio::providers::{
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
            "workspace_write",
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
        // …and it still parks the first time, because it does reach GitHub.
        assert_eq!(read.reach, Reach::Consequence);

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
        use openhuman_core::openhuman::memory_sync::composio::providers::{
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
    #[test]
    fn a_workspace_read_never_parks_whatever_its_name_begins_with() {
        for tool in [
            "file_read",
            "glob",
            "grep",
            "image_info",
            "list",
            "read_workspace_state",
            "memory_recall",
            "workspace_list",
            "workspace_read",
            "media_list_models",
            "composio_list_toolkits",
            "composio_list_connections",
            "composio_list_tools",
        ] {
            assert_eq!(c(tool).reach, Reach::Nothing, "`{tool}` is a read");
        }
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
        for tool in ["file_write", "memory_store", "shell", "workspace_write"] {
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
            workspace_tools::WORKSPACE_WRITE_TOOL,
            crate::harness::composio_catalog::LIST_TOOLS_TOOL,
            crate::harness::composio_catalog::LIST_TOOLKITS_TOOL,
        ] {
            assert!(
                DECLARED.iter().any(|d| d.tool == name),
                "`{name}` is a live tool constant with no declaration"
            );
        }
    }
}
