# MCP Servers (per-tenant tool servers)

Issue #50. Each company can expose remote **MCP tool servers** to its agents.
An agent granted a server reaches it through the generic bridge tools
(`mcp_list_servers`, `mcp_list_tools`, `mcp_call_tool`), reusing OpenHuman's
`mcp_client` registry, its HTTP transport, and its prompt-injection safety
filter over remote tool metadata.

Hosted v1 boundary: **HTTP transport only**. Out of scope for v1: stdio /
subprocess servers (rejected with a clear error), Smithery browsing,
MCP-server OAuth, and live pool invalidation.

## Where servers come from

A company's *effective* MCP servers are the union of two sources, merged by
name (a runtime entry overrides a manifest server of the same name but keeps
its `manifest` badge):

1. **Manifest** — `[[mcp_server]]` entries in `company.toml`
   ([`company::McpServer`](../../src/company/types.rs)). Declarative intent —
   an HTTP endpoint plus tool allow/deny lists and an optional *named* secret
   key — **never** an inline credential.

   ```toml
   [[mcp_server]]
   name = "notion"
   endpoint = "https://notion.example/mcp"
   allowed_tools = ["search", "read"]
   # auth_secret = "mcp/notion/auth"   # optional; names a SecretStore key
   ```

2. **Runtime** — servers the operator adds through the console, persisted as a
   single JSON index in the [`SecretStore`](../../src/ports/secrets.rs) under
   `mcp/servers`.

Validation (manifest + API): unique names, an `http(s)://` endpoint, and no
stdio `command`. See [`company::mcp`](../../src/company/mcp.rs).

## Credentials are write-only

A server's outbound token lives apart from its declaration, under the per-server
key `mcp/{name}/auth`. It is **write-only** over the API: set via the `token`
field on add/update, stored in the secret store, and **never** returned. The
read shape carries only an `authConfigured` boolean.

The agent-facing surface is redacted too: `OcMcpListServersTool`
([`harness::mcp`](../../src/harness/mcp.rs)) replaces OpenHuman's own
`mcp_list_servers` (which serializes bearer tokens into agent-visible output)
with a drop-in that emits the same shape minus any credential. A regression
test drives `mcp_call_tool` against an in-process MCP server and asserts the
bearer reaches the *server* over the wire but never appears in any `ToolResult`.

## Per-agent scoping

An agent reaches a server named `<slug>` only when its manifest `tools` grants
match `mcp:<slug>` — the same glob semantics as every other tool grant
(`mcp:*` grants all). `registry_for_agent` filters the resolved decls to the
enabled, granted set and folds them into a one-registry `oh::Config` with
`gitbooks.enabled = false` (so OpenHuman's default gitbooks server never leaks
into a tenant agent). An agent with no granted MCP server gets no bridge tools.

```toml
[[agent]]
id = "researcher"
role = "Researcher"
tools = ["mcp:notion", "mcp:linear"]   # or "mcp:*"
```

`mcp_call_tool` runs under a permissive OpenHuman `SecurityPolicy`; the
company's own `ApprovalPolicy` tool policy remains the real per-call gate.

## What parks for approval

`mcp_call_tool` parks under the default `supervised` mode and is denied under
`readonly`: it can perform any effect the remote server advertises, and it can
never be granted standing for the same reason.

`mcp_list_servers` and `mcp_list_tools` **never** park (issue #443). They read
local registration state with credentials already redacted and reach nothing.
This matters more than one saved prompt: the persona brief appended to every
MCP-granted agent *instructs* it to answer capability questions from a live
`mcp_list_servers` call rather than from memory, so while these parked, the
guidance written to stop stale answers could only be followed by interrupting an
operator. An agent's very first move parked, before it had done anything.

Both verdicts are declared in [`policy::consequence`](../../src/policy/consequence.rs)
alongside every other tool, and a test builds the live toolbelt and fails if a
wired tool has no declaration — so a new read-only bridge tool cannot quietly
start asking for permission.

## HTTP surface

Both scope forms are registered (`…/companies/{id}/…` and the single-company
alias `…/company/…`). See [`server::ops::mcp`](../../src/server/ops/mcp.rs).

| Method | Path | Purpose |
|--------|------|---------|
| `GET` | `…/mcp/servers` | Effective servers (`authConfigured`, never the token). |
| `POST` | `…/mcp/servers` | Add a runtime server (+ optional write-only `token`). |
| `PUT` | `…/mcp/servers/{name}` | Enable/disable, edit tool lists/endpoint, rotate token. A manifest server gets a runtime override entry. |
| `DELETE` | `…/mcp/servers/{name}` | Remove a runtime server. `409` for a manifest server (disable it instead). |
| `GET` | `…/mcp/servers/{name}/tools` | Live tool discovery through the registry. |

Discovery is gated on the `openhuman` feature (the MCP transport lives there);
without it the route reports `not_wired` and the console falls back to the
declared tool lists. Every mutating response carries a `note` reminder.

## Console surface

One component reads these routes —
[`McpServersSection`](../../frontend/src/views/connections/McpServersSection.tsx),
over the standalone functions in `frontend/src/api/mcp.ts` — rendered from two
places: inline on Connections, and as the whole of Settings, MCP Servers.

There is deliberately no MCP method on `OpenCompanyClient`. A second set used to
sit there, declaring a `{ servers }` wrapper around this table's bare array,
`server_id` keys, and `/connect` / `/disconnect` routes that exist nowhere; the
Settings page built on it crashed on open (issue #414). The client casts an
unparsed body to the declared type, so a second surface is never caught by the
compiler — only by whoever opens the page.

## Pool-staleness caveat

Agents materialize their MCP registry once, when the
[`HarnessPool`](../../src/harness/mod.rs) builds a company's roster. Mid-session
edits (add / disable / token rotation) therefore reach a live agent only on the
next `HarnessPool.ensure()` rebuild — practically, a company restart. Every
mutating API response says so. Live pool invalidation is out of scope for v1.
