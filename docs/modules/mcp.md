# MCP Servers (per-tenant tool servers)

Issue #50. Each company can expose remote **MCP tool servers** to its agents.
An agent granted a server reaches it through the generic bridge tools
(`mcp_list_servers`, `mcp_list_tools`, `mcp_call_tool`), reusing OpenHuman's
`mcp_client` registry, its HTTP transport, and its prompt-injection safety
filter over remote tool metadata.

Hosted v1 boundary: **HTTP transport only**. Stdio / subprocess servers are
rejected with a clear error — the tenant image ships no Node, Python or package
manager to launch one with. Still out of scope: live pool invalidation.

Directory browsing landed in issue #1270; see [The directory](#the-directory).

## Where servers come from

A company's *effective* MCP servers are the union of the sources below, merged
by name (a runtime entry overrides a manifest server of the same name but keeps
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

3. **Default** — `[[default_mcp_server]]` in the instance `config.toml`
   (issue #527): shipped by the install, present for every company, and badged
   `default` so it is never mistaken for something this operator added.

4. **Registry** — installed from an upstream MCP directory (issue #1270), badged
   `registry`. Keyed by a stable `serverId` rather than by name; see
   [The directory](#the-directory).

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

`mcp_call_tool` runs under a permissive OpenHuman `SecurityPolicy`. It is still
classified for audit, but policy-generated HITL is disabled.

## Approval behavior

`mcp_call_tool` does not automatically park under `supervised`. An agent that
needs sign-off calls `request_approval` explicitly before invoking it.
`readonly` remains a hard denial.

`mcp_list_servers` and `mcp_list_tools` do not require approval. They read
local registration state with credentials already redacted and reach nothing.
This matters more than one saved prompt: the persona brief appended to every
MCP-granted agent *instructs* it to answer capability questions from a live
`mcp_list_servers` call rather than from memory. These reads must remain
uninterrupted so the guidance that prevents stale answers is usable on an
agent's first move.

The classifications remain declared in
[`policy::consequence`](../../src/policy/consequence.rs) for audit and for a
future policy-HITL mode.

## HTTP surface

Both scope forms are registered (`…/companies/{id}/…` and the single-company
alias `…/company/…`). See [`server::ops::mcp`](../../src/server/ops/mcp.rs).

| Method | Path | Purpose |
|--------|------|---------|
| `GET` | `…/mcp/servers` | Effective servers (`authConfigured`, never the token). |
| `POST` | `…/mcp/servers` | Add a runtime server (+ optional write-only `token`). |
| `PUT` | `…/mcp/servers/{name}` | Enable/disable, edit tool lists/endpoint, rotate token. A manifest server gets a runtime override entry. |
| `DELETE` | `…/mcp/servers/{name}` | Remove a server, dispatching on where it lives. `409` for a manifest or default server (disable it instead). |
| `GET` | `…/mcp/servers/{name}/tools` | Live tool discovery through the registry. |
| `GET` | `…/mcp/config` | The declared servers as one `mcp.json` document (credentials never echoed). |
| `PUT` | `…/mcp/config` | Replace the declared set from that document (admin-only). |
| `GET` | `…/mcp/registry/search?q=&page=&pageSize=` | Browse the upstream directories. |
| `GET` | `…/mcp/registry/entry?qualifiedName=` | One entry, with the install decision already made. |
| `POST` | `…/mcp/registry/install` | Install an entry (+ write-only `env` values) and connect it. |
| `POST` | `…/mcp/registry/{serverId}/connect` | Dial an installed server. |
| `POST` | `…/mcp/registry/{serverId}/disconnect` | Drop the live session, keeping the install. |
| `PUT` | `…/mcp/registry/{serverId}/env` | Rotate an install's credentials (write-only). |
| `DELETE` | `…/mcp/registry/{serverId}` | Uninstall. |

The `…/mcp/registry/…` routes are gated on the `mcp` feature and report
`not_wired` without it, matching `…/oauth/start`. Every registry **mutation**
takes the admin guard: an install hands *every* teammate a new set of callable
tools, so it settles what the company can reach. Browsing decides nothing and
takes the ordinary company scope.

Discovery is gated on the `openhuman` feature (the MCP transport lives there);
without it the route reports `not_wired` and the console falls back to the
declared tool lists. Every mutating response carries a `note` reminder.

## `mcp.json` — the same configuration as one document

Issue: the MCP console redesign. `…/mcp/config`
([`server::ops::mcp_config`](../../src/server/ops/mcp_config.rs)) reads and
writes the **same runtime index** the per-server routes above write, shaped like
the `mcpServers` block an operator already has in a desktop MCP config:

```json
{
  "mcpServers": {
    "notion": {
      "type": "http",
      "url": "https://notion.example/mcp",
      "enabled": true,
      "allowedTools": ["search"],
      "timeoutSecs": 30,
      "source": "manifest",
      "authConfigured": true
    }
  }
}
```

It is one store behind two spellings, not an import/export format: a save here
and a `PUT …/mcp/servers/{name}` land in the same place, so the rows and the file
cannot describe different configurations. Pasting a block of servers is one
action in the document and N form submissions on the rows, which is what the
surface is for.

The rules the shape cannot carry:

- **A write is a replace, not a merge.** A runtime server absent from the
  document is removed, with its credential and health cleared — the same removal
  `DELETE …/mcp/servers/{name}` performs.
- **A manifest or default server cannot be deleted by omission.** Its
  declaration lives in `company.toml` or the instance `config.toml`, so dropping
  the row would not remove it: the next resolution merges it straight back. The
  write is refused by name, and `"enabled": false` — which persists as an
  override — is the way to silence one.
- **An unedited entry writes no override.** An entry equal to its
  manifest/default declaration is skipped, so saving a document nobody edited is
  a genuine no-op rather than a silent conversion of every declared server into
  an operator override.
- **Credentials stay write-only.** `headers` (one header;
  `Authorization: Bearer …` is stored in the same slot the console's token field
  writes) is accepted on write and never echoed on read. An entry that arrives
  without `headers` leaves the stored credential **unchanged** — a round-trip
  cannot silently deauthenticate a server.
- **Registry installs are not in the document.** They live in OpenHuman's own
  store keyed by `serverId`, not in this company's index, and a name here
  addresses no install — so rendering them would invite an edit that does
  nothing. They stay on the rows with their own routes.

Local checking is deliberately thin
([`frontend/src/lib/mcp-json.ts`](../../frontend/src/lib/mcp-json.ts)): JSON-ness,
the `mcpServers` object, and a `url` per entry. Everything else is the host's
answer to give and is shown verbatim, because a console paraphrase of the host's
validation is one more thing that can fall out of step with it.

## The directory

Issue #1270. Before it, the tab could only contain what somebody already knew
the address of: an operator arrived with a URL or the list stayed empty. Nothing
in `src/server/` reached `McpRuntime`
([`harness::mcp`](../../src/harness/built_in/mcp.rs)), the wrapper over
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
declared list is what the *agents* reach — `registry_for_agent` builds each
agent's registry from it and scopes it by `mcp:<name>` grants. Nothing is lost:
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

### Per-agent scoping does not apply to installs

`harness::built_in::build` pushes the registry bridge tools onto **every**
agent's belt with no grant check, so every teammate can call every installed
server. Issue #1270 leaves that in place deliberately and makes it visible: a
registry row's `reachableBy` lists the whole roster (and nobody when the install
is disabled) rather than claiming a scope the harness does not apply.

## Which builds can honour a server (issue #567)

The management routes above are **ungated** — they ship in every build. The
agent-side bridge is not: `registry_for_agent` is pushed onto a teammate's belt
behind `#[cfg(feature = "mcp")]`. Three configurations, only one of which the
routes alone distinguish:

| Build | CRUD | Discovery / probe | Agent tools |
|-------|------|-------------------|-------------|
| default (no `openhuman`) | works | `not_wired` | none — no harness |
| `openhuman`, no `mcp` | works | **works for real** | **none** |
| `openhuman` + `mcp` | works | works | yes |

The middle row is the one worth stating outright: every read on the screen
answers correctly, so a healthy badge and a live tool list sit above a server no
teammate can call. The console cannot infer this — an empty tool belt is not
visible over HTTP — so `GET …/capabilities` carries **`mcpInBuild`**
(`cfg!(feature = "mcp")`, alongside `mediaInBuild` / `composioInBuild` /
`searchInBuild`), and `McpServersSection` renders a stated degraded state when it
is explicitly `false`. A host that omits the field is *unknown*, never
"absent" — an older build must not be reported as broken.

Writes stay open on every build deliberately. A manifest can declare servers for
a deployment that runs elsewhere with the feature, and configuration entered
before the capability arrives survives the rebuild; refusing the write would turn
that into a hard error while fixing nothing an operator can act on. Staging
builds with `mcp` (`TENANT_FEATURES` in `deploy-staging.yml`); the default
`docker-compose` build does not.

## Console surface

One component reads the server routes —
[`McpServersSection`](../../frontend/src/views/connections/McpServersSection.tsx),
over the standalone functions in `frontend/src/api/mcp.ts` (List A) and
`frontend/src/api/mcp-registry.ts` (the directory) — rendered from two places:
inline on Connections, and as the **Connections** tab of Settings, MCP Servers.

That page ([`McpServersView`](../../frontend/src/views/McpServersView.tsx)) has a
second tab, **mcp.json**
([`McpJsonEditor`](../../frontend/src/views/mcp/McpJsonEditor.tsx)), over
`…/mcp/config`. Two tabs rather than two pages because they are not two things:
both go through the same host into the same store, so an edit in one shows up in
the other on its next read. A save bumps the key the rows are mounted on, so the
list re-reads rather than describing the configuration as it was before the file
was written.

A row's controls are icons, each carrying its sentence as a tooltip **and** as
its accessible name ([`McpIconButton`](../../frontend/src/views/mcp/McpIconButton.tsx)):
credential (sign in / add a token / set env credentials), connect or disconnect,
re-check, list tools, enable or disable, remove. At labelled-button width a row
six controls deep wrapped onto a second line, and the line it pushed off was the
one carrying the endpoint — the row's own information lost to its chrome. The
enable control being an icon is why a disabled server also says `disabled` in
words beside its badges: an icon in an off state reads as "press to turn off" as
readily as the reverse.

There is deliberately no MCP method on `OpenCompanyClient`. A second set used to
sit there, declaring a `{ servers }` wrapper around this table's bare array,
`server_id` keys, and `/connect` / `/disconnect` routes that exist nowhere; the
Settings page built on it crashed on open (issue #414). The client casts an
unparsed body to the declared type, so a second surface is never caught by the
compiler — only by whoever opens the page.

### Browsing the directory

[`McpRegistryBrowser`](../../frontend/src/views/connections/McpRegistryBrowser.tsx)
sits inside the same card as the add-a-URL form, under the same manage gate
(issue #403 — an install hands every teammate a new set of tools). What it
installs lands in the list above it with a `registry` badge; there is no second
section, for the reason the whole merge exists.

An entry's install form is exactly the `requiredEnvKeys` the host derived from
the connection the install will use, as password fields. Those values are
write-only in both directions: nothing sends one back, and the merged row
reports only `authConfigured`.

Its failures are its own. Both upstream directories are network hops and either
can be down, and on a build without the `mcp` feature every `…/mcp/registry/…`
route answers `404 not_wired` — so `registryOutage` in
`frontend/src/lib/mcp-registry.ts` turns *every* rejection into one of two
notices rendered inside the panel, and never rethrows. A dead directory is an
empty result with a reason; a missing feature is a sentence about the build. The
company's installed servers keep rendering through both.

### Provenance picks the routes, not just the badge

A row's `source` decides which half of the API it may call. List A's
enable/disable, re-check and tools controls resolve the row's `name` against the
declared list; a
directory install has no declaration and its `name` is a slug the merge minted,
so all three answer `no MCP server named …` on it. The registry's
connect / disconnect stand in their place, its delete is
`DELETE …/mcp/registry/{serverId}`, and its credentials rotate through
`PUT …/mcp/registry/{serverId}/env` rather than List A's single token field.

`mcpRowControls` in `frontend/src/lib/mcp-registry.ts` is the one place that
decides all four, and it reads `source` — never the presence of `serverId`. A
reconciled row carries a `serverId` and is still a manifest server: it keeps
List A's controls, keeps its badge, and keeps its refusal to be deleted.

One wire gap worth knowing: `GET …/mcp/servers` reports *that* a credential is
stored, never which keys hold it, so the rotation form re-reads the field names
from the catalogue entry. A directory outage therefore costs the rotation form
its fields even though `PUT …/env` is healthy — the form says so rather than
guessing.

### Opening one server

A row's name opens the server into
[`ProviderDetail`](../../frontend/src/views/connections/ProviderDetail.tsx) — the
same panel a Composio provider opens into (issues #404, #821), on a
`ConnectionSubject` union rather than a second MCP-specific panel. The reason is
the paragraph above one level up: two surfaces describing the same idea acquire
two vocabularies and then drift.

The panel is read-only. Enable, `Test`, `Tools` and `Remove` stay on the row;
what it adds is what the row cannot say. Its provenance and removal prose are
`mcpProvenanceNote` / `mcpRemovalNote` — one sentence per source, because the
panel used to read `manifest` against everything-else and told a directory
install it "was added from the console and lives in this company's runtime
store", true of neither half of it.

- **Connected, and as what.** MCP has no connection object, so this is assembled
  from two facts a single badge would collapse: `enabled` (whether any agent
  receives the tools at all) and the last probe (whether the endpoint answered
  when someone last asked). A server nobody has pressed `Test` on has no `health`
  at all, and "never probed" is neither reachable nor broken. See `mcpStanding`
  in `frontend/src/lib/connection-detail.ts`.
- **Usage**, read from `byProvider` under the `mcp:<server>` key this module's
  metering records (`src/metering/oauth.rs`) — never the bare slug, which is the
  same-named Composio toolkit's row.
- **No connection date**, stated rather than left blank. There is no connect step
  to record one; the probe timestamp the host *does* keep sits beside it.
- **What a disconnect reaches**: the tool belt on the next turn, and nothing at
  the server's own end. A manifest server says it cannot be removed at all.

## Pool-staleness caveat

Agents materialize their MCP registry once, when the
[`HarnessPool`](../../src/harness/mod.rs) builds a company's roster. Mid-session
edits (add / disable / token rotation) therefore reach a live agent only on the
next `HarnessPool.ensure()` rebuild — practically, a company restart. Every
mutating API response says so. Live pool invalidation is out of scope for v1.
