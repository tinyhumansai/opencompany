# Prerequisites

Four fixes. (a) has to land before the tiered-permission work in
`02-tool-tier-model.md`; (d) is free and should ship immediately regardless of
everything else; (b) and (c) are independent and can land whenever.

## (a) Per-server scoping for `mcp_registry` grants — land this first

**The gap, precisely.** `docs/modules/mcp.md`'s own "Per-agent scoping applies
to installs, gated by an explicit grant" section already documents this
honestly: `grants_mcp_registry_explicit`
([`company/types.rs`](../../../crates/opencompany-core/src/company/types.rs))
accepts a `mcp_registry.<sub>` grant string as syntactically valid — but at
wiring time (`harness/built_in/build.rs`, around lines 412–437) the check that
actually gates the tools is a bare boolean: does this agent's grant list
satisfy `grants_mcp_registry_explicit` at all, yes or no. On `true` it
constructs the registry tool-call bridge against the company's *entire*
installed-and-connected server set. The `.notion` suffix on a grant is parsed,
then discarded. An agent granted `mcp_registry.notion` and one granted bare
`mcp_registry` are wired the identical tool, reaching every installed server —
not just the one the operator meant to scope it to.

This is not a fresh discovery — it traces back to issue #1270, which shipped
the directory feature with this asymmetry deliberately deferred rather than
inventing a grant filter the harness did not apply at the time. It has stayed
that way since.

**Why it has to land before `02-tool-tier-model.md`.** The tiered-permission
model stores policy keyed by `(server_identity, tool_name)`. For a declared
server, `server_identity` is the server's `name`, and that's already
agent-scoped correctly via `mcp:<name>` grants (`runtime/tools.rs`'s
`grants_cover_server`). For a registry-installed server, `server_identity` is
`server_id` — and if reach isn't actually scoped per-server first, a per-tool
tier assignment on that server means nothing: every agent holding any
`mcp_registry` grant sees the identical tier data for a server some of them
should never have been able to name in the first place. Fix the reach question
before building a finer-grained answer on top of it.

**The fix, two parts:**

1. **A new predicate**, mirroring `grants_cover_server`
   (`runtime/tools.rs`). That function already builds `format!("mcp:{name}")`
   and checks it against the agent's grant list through the existing glob
   matcher (`grant_matches`, which already treats `.` as a namespace
   boundary). Add a sibling:

   ```rust
   // runtime/tools.rs, beside grants_cover_server
   pub(crate) fn grants_cover_registry_server(grants: &[String], server_id: &str) -> bool {
       let want = format!("mcp_registry.{server_id}");
       grants
           .iter()
           .filter(|g| g.as_str() != "*")
           .any(|g| g.as_str() == "mcp_registry" || grant_matches(g, &want))
   }
   ```

   The `g.as_str() == "mcp_registry"` arm preserves today's behavior for any
   agent still using the bare grant — back-compat, no behavior change for
   anyone not using the new scoped form. A new `mcp_registry.notion` grant
   narrows reach to just that `server_id`; `mcp_registry.*` continues to mean
   all, matching the existing scoped-grant test fixture at
   `runtime/builder_tests_scoped_grants.rs`.

2. **A wrapper tool**, mirroring the existing `OcMcpCallTool`
   (`harness/built_in/mcp.rs`). The vendored `McpRegistryToolCallTool` (from
   OpenHuman's `tinymcp` crate) takes no per-agent scoping parameter — it's
   shared, company-wide infrastructure, not agent-scoped the way
   `McpServerRegistry` is for declared servers. Rather than modify vendored
   code, add `OcMcpRegistryToolCallTool` in `harness/built_in/mcp.rs`,
   structured like `OcMcpCallTool`: it wraps the real vendored tool, and in
   its `execute_with_options`, extracts `server_id` from the call arguments
   (the same key the vendored tool's own schema already requires) and calls
   `grants_cover_registry_server` **before** delegating to the wrapped tool.
   On a miss: return an error result naming the missing grant — do not
   silently return an empty result, and do not park it as an approval. A
   scope violation is a configuration problem to surface loudly, not a
   consequence to review.

   Wire this at the same call site in `build.rs` that currently pushes the
   raw vendored tool — construct the wrapper with the agent's `grants`
   (already in scope there) instead.

**Size:** small — one new function, one new wrapper struct of roughly the same
size as `OcMcpCallTool`, one call-site change. No data migration: existing
bare `mcp_registry` grants keep their current meaning, so this is additive.

## (b) `mcp_call_tool` vs `mcp_registry_tool_call` — two tool names, one job

**Not a gating problem.** Confirmed: both tools already funnel through one
shared approval-gate function, `mcp_call_reach`
(`policy/consequence.rs`), consulting one `McpReadSet`. There is no missing
human-in-the-loop primitive on the registry-install path — the split is purely
about which name an agent has to already know to call.

**What it actually costs.** An agent must know, before making the call,
whether a given connected server came from the declared/manifest/console path
(`mcp_call_tool`) or the directory-install path (`mcp_registry_tool_call`).
Nothing today cross-references the two catalogues for the model — it has to
already know which family a server belongs to.

**Recommendation: don't unify the tool names as part of this effort.**
Collapsing them into one dispatch tool means real backend work — either a
single tool that branches internally on which store a `server` identifier
resolves against (touching both `McpServerRegistry` and the registry's
`McpRuntime`/`Config`), or migrating registry installs into the declared
server data model entirely (rejected in `03-storage-api-rollout.md` for
storage-migration cost reasons). That's disproportionate to what's actually
wrong here.

**Cheap mitigation instead, unordered relative to everything else in this
brief:** `mcp_list_servers`'s output is already reconciled/merged for the
console (`server/ops/mcp_registry/wired.rs`'s `merge_installs`). Feed that
same reconciled view into the tool description or the system prompt so the
model is told, once per turn, which family a connected server belongs to —
a prompt change, not a schema change. File as a follow-up; it doesn't block
anything else in this brief.

## (c) The `mcp` build-feature gap

**Confirmed still present.** `server/setup.rs`'s `mcp_in_build:
cfg!(feature = "mcp")` (and its mirror in the capabilities response) is a
signal completely separate from whether `server/ops/mcp.rs`'s CRUD and probe
routes actually function — those routes compile and run in every build
regardless of the `mcp` feature. A build without `mcp` can add a server, get a
genuinely live, green `Test connection` result, and still wire **zero**
agents to it, because the agent-side bridge tools in `build.rs` are gated
behind `#[cfg(feature = "mcp")]`. Nothing on the backend refuses to run a
probe whose result the build can't act on.

**Fix:** in `server/ops/mcp.rs`'s `test_server`, `add_server`, and
`update_server` handlers, short-circuit to a typed non-functional response
(reuse `McpStatus`'s `Unknown` variant with a specific message, or add a new
variant if `Unknown` is already overloaded elsewhere) when
`!cfg!(feature = "mcp")`, instead of running the live probe and reporting a
result the build structurally cannot use.

**Sequencing:** independent of (a)/(b)/(d) — no dependency either direction.
Bundle it into the same PR as `03-storage-api-rollout.md`'s new API endpoints
since both touch `server/ops/mcp.rs`; that's a convenience, not a requirement.

## (d) `docs/modules/mcp.md`'s stale restart claim — ship immediately, alone

**Confirmed wrong, and confirmed what's actually true**, by reading the code
directly (not inferring from an earlier doc pass): `docs/modules/mcp.md`'s
"Pool-staleness caveat" section says a mid-session MCP edit reaches a live
agent only on the next `HarnessPool::ensure()` rebuild, "practically, a
company restart," and calls live invalidation "out of scope for v1." That's
stale. `harness/built_in/mod.rs` (`HarnessPool`) maintains a
`mcp_fingerprints: RwLock<HashMap<CompanyId, u64>>` field, alongside sibling
fingerprint maps for overlay agents, capabilities, Composio config, skills,
budgets, grants, and more. `ensure_with_policy` computes `mcp_fp =
mcp_fingerprint(&effective_mcp)` fresh on every call and compares it against
the cached value before deciding whether to reuse the existing roster or
rebuild — an MCP config change changes the fingerprint, which forces a
rebuild on the **next turn**, not the next restart. The API layer already
knows this: `server/ops/mcp.rs`'s `NEXT_TURN_NOTE` constant, attached to every
mutating response, says "Agents pick up this change on their next turn — no
restart needed." The module doc just never caught up to the code.

**Fix:** rewrite `docs/modules/mcp.md`'s "Pool-staleness caveat" section (and
its early summary line, "Still out of scope: live pool invalidation") to
describe the fingerprint mechanism as it actually works, citing
`HarnessPool`'s fingerprint fields and `ensure_with_policy`'s comparison.

**Zero risk, zero dependency on anything else in this brief — do this one
first, alone, regardless of what order the rest lands in.**
