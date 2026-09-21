# Storage, API, and rollout

## Declared vs. registry storage — stays split; only the policy shape unifies

**Recommendation: do not merge the two backing stores.** `docs/modules/mcp.md`
already documents why they're separate today: declared servers' credentials
live in OpenCompany's own SecretStore (write-only, never echoed — a
load-bearing guarantee the module doc's "Credentials are write-only" section
treats as core to the design); registry installs' credentials and connection
lifecycle live in OpenHuman's own SQLite-backed store, owned by `McpRuntime`
(`harness/built_in/mcp.rs`), with its own connect/disconnect state machine
this repo depends on rather than reimplements.

The reconciliation that already happens — `server/ops/mcp_registry/wired.rs`'s
`merge_installs`, matching declared and installed servers by normalized
endpoint for display — works at *read time*, for the console's benefit. It
does not require, and never has required, one shared store underneath.
Unifying storage now would mean either migrating registry credentials into
the SecretStore (reimplementing whatever OpenHuman's SQLite side already
gives us for free around reconnect/health) or migrating declared servers into
OpenHuman's store (breaking the write-only guarantee). Neither is worth it
for what this brief actually needs.

**What does unify: the policy shape.** `McpToolPolicies` from
`02-tool-tier-model.md` is identical for both kinds of server — just filed at
two different SecretStore addresses (`mcp/{name}/tools` vs.
`mcp_registry/{server_id}/tools`). Policy was never what split the two
stores; credentials and connection lifecycle were. This is the narrowest
change that still delivers one console page working uniformly regardless of
which store a server's credential happens to live in.

## API / endpoint changes

Following the existing route conventions in `server/ops/mcp.rs`'s router
(`scoped("/mcp/servers/{name}/...", ...)`) and the registry's equivalent:

**New, declared servers:**
```
GET  /mcp/servers/{name}/tools/policy       → per-tool tier + mode, tier defaults
PUT  /mcp/servers/{name}/tools/policy       → bulk-set tier defaults and/or per-tool overrides
```

**New, registry-installed servers** (in `server/ops/mcp_registry/`, beside
the existing per-`{serverId}` routes):
```
GET  /mcp/registry/{serverId}/tools/policy
PUT  /mcp/registry/{serverId}/tools/policy
```

**Extend the existing discovery route** (`GET /mcp/servers/{name}/tools`,
today's live tool-discovery-through-the-registry endpoint) to also return
each tool's *suggested* tier from `02-tool-tier-model.md`'s
`suggest_tool_tier`, so the console can render the whole permissions page
from one round trip rather than two. New response field, additive,
`#[serde(skip_serializing_if)]`-guarded like the rest of `McpServerDto`'s
optional fields.

**DTO**, mirroring `McpServerDto`'s existing style:
```rust
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ToolPolicyDto {
    pub(super) name: String,
    pub(super) suggested_tier: ToolTier,   // from discovery — informational
    pub(super) effective_tier: ToolTier,   // suggested_tier unless the operator explicitly reclassified it
    pub(super) mode: ApprovalMode,          // resolved: override, else tier default, else hardcoded default
    pub(super) is_override: bool,           // whether `mode` came from an explicit per-tool entry — drives the "↺ reset" affordance in README's mockup
}
```

`PUT` bodies reuse the existing "every field optional, only set fields
applied" convention already used by the add/update-server handlers, for
bulk tier-default updates, plus a simple `{ tool: String, mode: ApprovalMode }`
list for per-tool overrides.

**Bundle with this PR:** the build-feature-gap fix from
`01-prerequisites.md` §c, since it touches the same handlers in
`server/ops/mcp.rs` — `test_server`/`add_server`/`update_server` should
short-circuit to a typed non-functional response when `!cfg!(feature =
"mcp")`, instead of running a live probe whose result a feature-less build
can't act on. Convenience bundling, not a hard dependency.

## What this unlocks for the console (brief)

Confirmed by reading it: the current `McpServersSection.tsx` has no per-tool
table at all today — nothing beyond a `reachableBy` roster line. The backend
design above fully supports the permissions page in `README.md`'s mockups,
but the frontend needs a genuinely new sub-view, not an extension of the
existing section. The natural home is alongside `ProviderDetail.tsx` — the
same generic connection-detail panel MCP and Composio servers already share
(`docs/modules/mcp.md`'s "Opening one server" section) — since that's the
existing precedent for "one connected thing, one detail view," not a new
MCP-specific panel. Actual screen implementation is separate work, out of
scope for this brief.

## Rollout order

1. **`docs/modules/mcp.md` fix** (`01-prerequisites.md` §d) — ship alone,
   immediately, zero risk, zero dependency on anything else here.
2. **Per-server registry grant scoping** (`01-prerequisites.md` §a) — ship
   alone. Additive: existing bare `mcp_registry` grants are unaffected: it
   only changes behavior for an agent later given a new scoped
   `mcp_registry.<id>` grant, which is nobody until an operator opts in.
   Blast radius: zero on upgrade.
3. **The tiered data model + migration + consequence rewrite**
   (`02-tool-tier-model.md`) — the one step that touches every company with
   any MCP server configured, so it carries the strongest gate: the
   byte-identical-behavior test is a hard merge requirement, not optional.
   Stage it in two commits rather than one: land the types and the
   migration function first, with `mcp_call_tool_consequence` still reading
   the legacy `read_only_tools` field directly and the new machinery inert;
   then land the switch-over to the new tiered lookup as a separate, small,
   easily-revertable commit once the migration path has actually run
   against real data in at least one environment. Blast radius: every
   company with `read_only_tools` set today gets lazily re-read with
   (per the migration proof) no behavior change; zero blast radius for
   companies with no MCP servers configured at all.
4. **Build-feature-gap fix + new API endpoints** — bundle together (same
   file, `server/ops/mcp.rs`). Depends on step 3 landing, since the new
   routes serve the tiered data. Blast radius: additive routes; the
   `mcp_in_build` short-circuit only changes behavior on non-`mcp`-feature
   builds, a minority configuration.
5. **Frontend** — depends on step 4's endpoints existing, but the page can
   be built and demoed against a local/staging backend before step 4 is
   fully rolled out everywhere, since it's an entirely new page rather than
   a modification of existing UI.
6. **Tool-naming prompt mitigation** (`01-prerequisites.md` §b) —
   unordered, independent of everything above, can land at any point.

**Explicitly not part of this rollout:** the MCP-native annotation work from
`02-tool-tier-model.md`'s closing section (the vendored `tinymcp` change).
It's an enhancement to suggestion quality, not a dependency of anything
above — file it separately once there's a concrete complaint to justify the
cross-repo work.
