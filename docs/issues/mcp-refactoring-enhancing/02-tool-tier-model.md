# The tool-tier data model

## Where a tool's tier comes from

MCP's own spec has a real answer here — `ToolAnnotations`
(`readOnlyHint`/`destructiveHint`/`idempotentHint`/`openWorldHint`) — and if
those hints were reachable, reading them would be the cheapest possible
source for tier classification. They are not reachable today, for two
separate reasons, and the second one matters even once the first is fixed.

**Not parsed today.** The client-side types OpenCompany actually receives a
remote server's tool list through — `McpRemoteTool` (wire-level,
`vendor/openhuman/vendor/tinymcp/crates/tinymcp-bus/src/transport/types.rs`)
and `McpTool` (registry-level, the registry crate's `types.rs`) — carry only
`name`, `title`/`description`, and `input_schema`. Neither has an
`annotations` field. Both are plain `#[derive(Deserialize)]` structs with no
catch-all field, so a remote server's `annotations` object is silently
dropped by `serde_json` at the parse boundary today — it isn't hidden
somewhere reachable, it's discarded. Getting it means adding the field to
`McpRemoteTool` and threading it through to `McpTool`, in the vendored
`tinymcp` crate two levels down from this repo (`vendor/openhuman`'s own
vendored dependency) — real, but cross-repo, work this repo can't land
unilaterally.

**Shouldn't be authoritative even once available.** MCP's spec is explicit
that these are *hints*, not guarantees, self-reported by whoever runs the
remote server. A directory-installed server from the public, unvetted
`modelcontextprotocol/registry` — see `docs/modules/mcp.md`'s "The
directory" section — could claim `readOnlyHint: true` on a tool that deletes
data. Keying an approval *bypass* off a value the remote party controls is
the wrong trust boundary — the same reasoning `mcp_call_tool_consequence`'s
existing doc comments already give for why the base case has to be
`Reach::Consequence` (fail closed) rather than trusting anything about the
call by default.

**So: two layers**, matching what a real product does with this same
ambiguity (see `README.md`'s mockups — the "suggested: read-only" line next
to a still-editable three-way toggle on every row):

1. **Suggested tier** — non-authoritative, UI-only, computed once at
   discovery/probe time. Source: a heuristic over the tool's `name` +
   `description` — verb-prefix matching, the same kind of pattern-matching
   `read_only_tools`'s current entries already represent informally, just
   split into three buckets instead of one flat list. A `get_*`/`list_*`/
   `read_*`/`search_*` prefix suggests `ReadOnly`; a `delete_*`/`remove_*`/
   `drop_*` prefix suggests `WriteDelete`; everything else defaults to
   `Interactive`, the conservative middle tier. Lives as a function,
   `suggest_tool_tier(tool: &McpTool) -> ToolTier`, beside the existing
   `mcp_read_set` in `company/mcp.rs`. (The MCP-native-annotation source
   from above slots in as a *second* signal here later, once available —
   see the closing section of this file — it doesn't change anything about
   layer 2 below.)
2. **Operator override** — authoritative, persisted, what the approval gate
   actually reads. Defaults to "inherit the tier's bulk setting" until the
   operator explicitly touches that row, exactly like the mockup shows
   "search_pages" pre-selected to `Allow` with no `↺ reset` marker, versus
   "move_page" showing one because someone changed it.

This means the classifier in layer 1 never has to be perfect — it's a
starting point the operator corrects once. That's also, visibly, what a real
product does with this same problem: someone pre-classified the ~26 tools in
README's Figma-derived mockup; the operator audited it, not built the
taxonomy from nothing.

## The types

New, in `company/mcp.rs`:

```rust
/// A remote tool's risk tier — informational/suggested unless an operator
/// has confirmed it. Never derived from a server's own self-reported hints
/// without that confirmation; see this file's "Where a tool's tier comes
/// from" for why.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolTier {
    Interactive,
    ReadOnly,
    WriteDelete,
}

/// What happens when this tool is called, whatever tier it's classified
/// under.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalMode {
    AlwaysAllow,
    NeedsApproval,
    Blocked,
}

/// One remote tool's resolved policy: its tier and the operator's decision.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolPolicy {
    pub tier: ToolTier,
    pub mode: ApprovalMode,
}

/// Per-server tool policy: an explicit per-tool override map, plus per-tier
/// bulk defaults so "everything in Read-only" doesn't need N identical
/// entries.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpToolPolicies {
    /// Per-tier default, applied to any tool of that tier with no explicit
    /// override. A missing tier falls to the hardcoded default: ReadOnly →
    /// AlwaysAllow, Interactive → NeedsApproval, WriteDelete →
    /// NeedsApproval (matching the mockup's observed per-tier defaults).
    #[serde(default)]
    pub tier_defaults: std::collections::HashMap<ToolTier, ApprovalMode>,
    /// Explicit per-tool override, keyed by remote tool name. Wins over
    /// `tier_defaults` when present.
    #[serde(default)]
    pub overrides: std::collections::HashMap<String, ToolPolicy>,
}
```

**Storage.** A new SecretStore key, `mcp/{name}/tools`, mirroring the
existing `auth_key`/`health_key` naming already in `company/mcp.rs`, holding
`McpToolPolicies` as JSON. Not a secret in the credential sense — reusing
this store because it's already the per-server keyed store every other piece
of this server's state lives in, and introducing a second store type for one
more field would just reproduce the two-backing-store situation
`03-storage-api-rollout.md` already has to reason about for a different
axis. Add `pub fn tools_key(name: &str) -> String { format!("mcp/{name}/tools") }`
beside the existing `auth_key`/`health_key` functions.

`InferenceDecl`-equivalent (`McpServerDecl`) gains `pub tool_policies:
McpToolPolicies`, loaded by `resolve_effective` the same way `auth` and
`read_only_tools` already are. The flat `read_only_tools: Vec<String>` field
**stays** on `McpServer`/`McpServerDecl` — it becomes a migration input (see
below), not something new code writes to.

For registry-installed servers, the identical `McpToolPolicies` shape
applies, keyed by `server_id` instead of `name`, stored at
`mcp_registry/{server_id}/tools` in the same SecretStore — see
`03-storage-api-rollout.md` for why policy storage doesn't need to follow
credential storage's split between the two server kinds.

## The `mcp_call_tool_consequence` rewrite

Replace the current binary `McpReadSet` lookup with a three-way tier lookup:

```
fn mcp_call_reach(tool_name, args, policies: &McpToolPolicySet) -> Consequence:
    base = mcp_call_tool_consequence(args)   // unchanged fail-closed base
    (server, remote_tool) = mcp_call_pair(tool_name, args)?  // unchanged extraction
    policy = policies.lookup(server, remote_tool)  // resolves: override, else tier default, else hardcoded tier default
    match policy.mode:
        AlwaysAllow  => Consequence { reach: ExternalRead, standing: PerCall }   // same downgrade as today
        Blocked      => refuse before the call reaches the transport — see below
        NeedsApproval => base   // unchanged: Reach::Consequence, parks under supervised/auto
    // no policy found for this (server, tool) => base, same fail-closed default as today
```

**`Blocked` is a genuinely new enforcement class, not a `Reach` variant.**
Today `Reach` only ever answers *how much scrutiny before running* — it has
no concept of *whether a call is allowed to run at all* (that's the grant
system's job, one layer up, and it's per-server, not per-tool). Two ways to
implement a real refusal:

- **(Recommended) Check `Blocked` inside the tool wrapper**
  (`OcMcpCallTool` / the new `OcMcpRegistryToolCallTool` from
  `01-prerequisites.md` §a) before calling `registry.call_tool` / delegating
  — return an error result directly, no network call made. Keeps `Reach`'s
  meaning unchanged (purely about approval-parking) and keeps the refusal
  next to where credentials and the network call actually happen — the same
  place `OcMcpCallTool::handle_failure` already owns this tool family's
  agent-facing error text.
- **(Rejected) Add a `Reach::Blocked` variant** threaded through the whole
  policy dispatch table in `policy/consequence.rs`. Rejected because `Reach`
  is consulted by every tool in that table, and a variant only one tool
  family would ever produce is exactly the kind of narrow, table-wide edit
  that module's own comments already warn against for changes with limited
  applicability.

`McpReadSet` itself doesn't need to change shape — it's a generic
`(server, tool) → is-a-read` set with other consumers. What changes is what
*populates* it: the current `mcp_read_set` function (`company/mcp.rs`)
flattens `read_only_tools`; it gets replaced by a richer flattener reading
`McpToolPolicies` and producing the same `(server, tool)` pairs for every
tool resolving to `AlwaysAllow`. `Blocked` tools are handled entirely inside
the wrapper, per the note above, not through `McpReadSet` at all.

## Migrating existing `read_only_tools` data

Lazy, per-server, on first read after upgrade — not a bulk migration script:

```
if mcp/{name}/tools is absent (this server has never had tiered policy set):
    for each tool in the legacy read_only_tools list:
        overrides[tool] = ToolPolicy { tier: ReadOnly, mode: AlwaysAllow }
    tier_defaults = {}  // use the hardcoded defaults for everything else
    write this to mcp/{name}/tools once, so it's stable on every subsequent read
```

**This must be a pure restatement, not a behavior change, and that's a hard
merge gate, not a nice-to-have.** Today, a `read_only_tools` member gets the
`ExternalRead` downgrade; everything else parks (`Reach::Consequence`). The
migrated model produces `AlwaysAllow` (→ `ExternalRead`, identical) for the
same members, and falls to the hardcoded `Interactive` default
(`NeedsApproval` → base `Reach::Consequence`) for everything else — the same
parking behavior as today, just restated through the new model. **No
company's live approval behavior may change on upgrade.** Before merging the
switch-over described in `03-storage-api-rollout.md`'s rollout plan, add a
test that asserts `mcp_call_reach`'s output is unchanged for every
`(server, tool)` pair, before and after migration, on a representative
fixture covering at least: a tool in the legacy `read_only_tools` list, a
tool not in it, and a server with an empty `read_only_tools` list.

## Picking this back up later: MCP-native annotations

Once the vendored `tinymcp` change from this file's opening section lands
(a separate, cross-repo piece of work — not part of this brief's rollout),
`suggest_tool_tier` gains a second signal: a server's own
`readOnlyHint`/`destructiveHint`, folded in *alongside* the name/description
heuristic, still only ever feeding the **suggested** tier, never the
enforced one. File this as its own follow-up once there's a concrete "the
name-pattern heuristic gets server X wrong" complaint to justify the
cross-repo work — it's an enhancement to suggestion quality, not a
dependency of anything in `03-storage-api-rollout.md`'s rollout plan.
