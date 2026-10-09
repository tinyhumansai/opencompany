# Directory-installed MCP servers

Split out of [MCP Servers](mcp.md), which holds the declared half: where a
`[[mcp_server]]` comes from, how its credential is stored, how agents are scoped
to it, and the console that manages it. This page is the **directory** half —
servers a company installs from the MCP registry rather than declaring, and
which are addressed by an install id rather than by name.

## The directory

Issue #1270. Before it, the tab could only contain what somebody already knew
the address of: an operator arrived with a URL or the list stayed empty. Nothing
in `src/server/` reached `McpRuntime`
([`mcp::runtime`](../../src/mcp/runtime.rs)), the wrapper over
OpenHuman's own MCP registry — the open `modelcontextprotocol/registry`, a
SQLite store of installs, named write-only env credentials, boot-time connect and
a supervisor — even though it is constructed for every company.

[`server::ops::mcp_registry`](../../src/server/ops/mcp_registry.rs) is that
routing layer.

### One list, not two sections

`GET …/mcp/servers` returns declared servers **and** directory installs as one
list, each row badged with its provenance. A server present in both — installed
from the directory *and* typed in by URL — is **one reconciled row**, matched on
the normalised endpoint: lowercased scheme and host, default port dropped, query
and fragment stripped, trailing slash dropped.

The query string has to go, because a declared server may carry its credential
as a query parameter; a comparison that kept it would never match, and the
operator would get the same server twice with two credentials and two health
badges disagreeing.

**The declared side wins the provenance.** `source` decides the badge and
whether the console offers a delete, and both must answer to the declared list: a
manifest server cannot be deleted, only disabled, so an install must not be able
to capture that row and relabel it deletable. The deeper reason is that the
declared list is what the *agents* reach — `resolve_for_agent` attaches it to
each agent's spec, scoped by `mcp:<name>` grants. Nothing is lost:
`serverId` rides on the reconciled row, so the registry routes still address it.

The registry contributes only what the declared side has no field for —
`serverId`, `qualifiedName`, `iconUrl`, `transport`, a `description` where there
was none, and a `health` where the server has never been probed (a real probe
wins, since it dials the way the agents' bridge tools do). `authConfigured` is
the union. All four registry fields are omitted when absent, so a declared row's
JSON is byte-identical to what it was before this existed.

### One directory, and no key to keep

The browse surface queries the open `modelcontextprotocol/registry` and nothing
else. Entries declaring no remote endpoint are discarded by the
hosted-transport filter — correctly: this deployment launches no local
subprocess — so what an operator sees is what this host can actually dial.

**Smithery was the other half and was removed.** It carried more hosted servers,
but upstream adds it only when an API key resolves, so it came with a
per-company credential slot on a console tab: a key to store write-only, rotate,
clear, explain two working tiers of (its own vs one host-wide account shared by
every tenant), and answer support questions about. A directory that needs a
credential before it shows anything is a directory that reads as broken until
somebody pays for it. What remains needs nothing, and a server the registry does
not list is still one paste of a URL away — which is how every declared server
got there before the directory existed at all.

Upstream still reads a host-process `SMITHERY_API_KEY` if one is set; nothing in
this deployment writes, reads or reports it.

### Ranking and top connectors

The registry search route
([`mcp_registry::wired`](../../crates/opencompany-core/src/server/ops/mcp_registry/wired.rs))
orders what it returns with the pure helpers in
[`mcp_registry::catalogue`](../../crates/opencompany-core/src/server/ops/mcp_registry/catalogue.rs):

- `rank_catalogue` marks an entry `official` only when its qualified name is in
  `tinymcp::registry::curation::OFFICIAL_SERVERS`, matched exactly, then sorts
  official first and by install count after that. Ties keep upstream's order.
  The upstream registry reports every entry as unofficial, so this list is the
  only source of the verified badge. Install counts come only from Smithery, so
  on the official registry the second key orders nothing.
- A browse — no search term — opens on **top connectors**: the official
  connectors alone, each fetched by name and kept when it names an endpoint this
  host can dial (`featured_entry`). "Show more" continues into the directory,
  numbered after that first page (`browse_upstream_page`, `shift_browse_page`)
  and without repeating an official entry. The official registry records no
  install or download counts, so its own order — by publisher domain — is not a
  ranking worth leading with.

### Icons are inlined host-side

An icon reaches the console only as a `data:image/` URI. Search, detail and the
install rows pass every icon through `inline_icon`: an inline image is kept, a
remote `http(s)` address is fetched by the host through the SSRF-guarded
`fetch_icon` (64 KB cap, 5 s timeout) and inlined, and anything else is dropped.
Results are cached per URL (512 entries). The console renders nothing that is
not already inline, so opening Discover makes no request to a publisher's host
from the operator's browser.

The registry gives most first-party servers no title and no icon, so the
official thirteen are named, and given their GitHub organisation avatar as a
logo, from the brand table in `catalogue`. Any other entry whose name is only
`mcp` or `server` is shown under its publisher namespace (`com.acme/mcp` →
"Acme").

### Delete dispatches

`DELETE …/mcp/servers/{name}` removes what the row actually has: the
runtime-index entry, the upstream install, or **both** for a reconciled row.
Dropping only the index row there would leave the install connected with its
tools still on every belt — a delete the operator watches fail. Manifest and
default rows stay `409`.

### Hosted transport only

Directory search is pinned to the hosted-transport filter, so a stdio-only entry
never reaches the operator's screen; the install route refuses one again by name,
because a caller can POST a qualified name search never offered. The blocker is
not the read-only root filesystem — tenants mount a writable `/data` — but that
the runtime image is `debian:bookworm-slim` plus `ca-certificates`, `curl`,
`libssl3` and X11 libs. A stdio install would fail on `npx: not found`.

### Nothing crosses the wire blind

Env values are write-only exactly like a declared server's `token`. Upstream's
catalogue DTOs end in a flattened `extra` map that round-trips every key the
registries emit, so each projection names the fields it forwards. An install's
raw `last_error` is **dropped**, not scrubbed: the scrubber's redaction pass
needs the credential values to replace, and this surface deliberately never
loads them — only the stable `auth_hint` code and a fixed sentence per status
cross the wire.

### A failing registry does not break the read

An unreadable store or a directory that will not answer resolves to "no
installs", and `GET …/mcp/servers` still returns the declared list. The declared
half is what governs what the agents reach, so it is the half that must survive.

### Per-agent scoping applies to installs, gated by an explicit grant

`harness::built_in::build` wires the registry bridge tools
(`mcp_registry_list_tools` / `mcp_registry_tool_call`) onto an agent's belt only
when its effective grants explicitly include `mcp_registry` (or a
`mcp_registry.<sub>` grant) — see
[`grants_mcp_registry_explicit`](../../src/company/types.rs). A catch-all `*`
does **not** confer it, the same rule as `composio`/`media`/`search`. Granted
but no registry home configured wires nothing (fail-closed) rather than
erroring.

Both tools address an install by a `server_id` argument at call time, so being
wired is not the whole gate. Each is wrapped in `OcMcpRegistryScopedTool`
(`mcp::agent`), which resolves that argument against the agent's effective
grants through `grants_cover_registry_server`
([`runtime/tools.rs`](../../src/runtime/tools.rs)) before delegating:

| Grant | Reaches |
|---|---|
| `mcp_registry` | every install (what the grant has always meant) |
| `mcp_registry.*` | every install |
| `mcp_registry.<server_id>` | that install only |
| `*` alone | nothing — MCP stays an explicit opt-in |
| `mcp*` | nothing — that grant is written for the `mcp:<server>` bridge |

`mcp*` is worth its own row because the shared grant matcher treats `_` as a
namespace boundary, so it would otherwise span from the bridge namespace into
this one and reach every third-party install. `grants_mcp_registry_explicit` —
which decides whether these tools are wired at all — accepts only a grant rooted
at `mcp_registry`, and the scoping predicate matches it, so the two gates cannot
come to disagree about what confers the namespace.

A `mcp_registry_tool_call` result also carries the same `McpCallOutcome` the
native `mcp_call_tool` attaches: answered when the install replied, failed with
the error's wire code when the vendored tool reports a string error body, and
`ToolNotAllowed` on the decorator's own refusals. The MCP call observer reads it
like any other call, so a directory install's answered calls are metered as
`OauthCall` under `mcp:<server_id>` and its failures reach the operator bubble
and the journal — see [What a call reports back](mcp.md#what-a-call-reports-back).

### The agent learns which installs exist from an allowlist

A `server_id` has to come from somewhere, so `mcp_registry_installed_list` is
wired beside the two bridge tools under the same grant — OpenCompany's own tool,
not OpenHuman's, whose answer is the install record serialised whole: the dial
string (`transport`, `command`, `args`) and the opaque `config` blob, where an
HTTP-remote URL can carry a query-parameter credential and a stdio install's
arguments a flag one.

The answer here is an allowlist — install id, qualified and display name,
description, enabled, the transport **kind** alone, last connection — so a field
added upstream must be opted in rather than arriving on the agent's side
unnoticed. It is scoped by the predicate the call path uses, so enumeration
cannot be the way around the grant.

A call naming an install the grants do not cover is refused with an error
result naming the grant that would allow it; the inner tool is never reached
and nothing is dialled. A call whose `server_id` is missing or blank is refused
separately, so a malformed argument does not read to the agent as a missing
grant.

**`<server_id>` is the install's own identifier — a UUID minted when the server
is installed — not its qualified or display name.** A scoped grant spells that
UUID, and reinstalling the same directory server mints a new one, which
silently retires a grant that named the old install. Resolving a friendlier
alias was considered and rejected: two installs can normalise to the same slug,
which would widen a permission boundary without anyone seeing it, and display
names are operator-mutable.

A tool that *enumerates* installs rather than addressing one carries no
`server_id` to gate on; such a tool must filter its rows through the same
predicate, the way `resolve_for_agent` filters declared servers with
`grants_cover_server`. `mcp_registry_installed_list` is the one that does.

A registry row's `reachableBy` follows this gate: it lists the agents whose
effective grants pass `grants_cover_registry_server` for that install (and
nobody while it is disabled), and its `accessGrant` is
`mcp_registry.<server_id>`.

## A slow or failing directory

The host bounds every directory read and answers a typed failure instead of the
upstream error: a search gets 8 s, then `504 registry_timeout`; any other
upstream failure is `503 registry_unavailable`. Both bodies are an operator
sentence with no upstream address or transport detail, which goes to the log.
Each featured connector lookup gets 4 s, and a page waits at most 1.5 s per icon:
the fetch carries on in the background and fills the cache for the next page,
and a failed icon address is left alone for 10 minutes.

In the console, a new search term aborts the request it replaces, keeps the
previous rows on screen (dimmed) under "Searching for …", and a failed search or
a failed **Show more** says so in place with a **Retry**. A search first answers
from the featured rows already loaded: every word of the query is matched,
case-insensitively, against the display name, qualified name and description,
and name matches come first. Those rows show at once while the live search runs,
lead the merged list when it lands (deduped by qualified name), and stay on
screen with "Showing popular matches — the MCP directory is slow right now." and
a **Retry** when the search answers `registry_timeout` or `registry_unavailable`.
Only a search with no featured match shows the error state. The entry pop-up will
not install while its directory lookup has failed, and closes once an install
succeeds.
