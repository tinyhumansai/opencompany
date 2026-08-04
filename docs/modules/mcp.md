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

## Pool-staleness caveat

Agents materialize their MCP registry once, when the
[`HarnessPool`](../../src/harness/mod.rs) builds a company's roster. Mid-session
edits (add / disable / token rotation) therefore reach a live agent only on the
next `HarnessPool.ensure()` rebuild — practically, a company restart. Every
mutating API response says so. Live pool invalidation is out of scope for v1.
