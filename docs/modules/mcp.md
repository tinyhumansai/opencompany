# MCP Servers (per-tenant tool servers)

Issue #50. Each company can expose remote **MCP tool servers** to its agents.
An agent granted a server reaches it through the generic bridge tools
(`mcp_list_tools`, `mcp_call_tool`), reusing OpenHuman's
`mcp_client` registry, its HTTP transport, and its prompt-injection safety
filter over remote tool metadata.

Hosted v1 boundary: **HTTP transport only**. Stdio / subprocess servers are
rejected with a clear error — the tenant image ships no Node, Python or package
manager to launch one with.

Directory browsing landed in issue #1270; see [The directory](#the-directory).

The code lives under `src/mcp/` — declarations, per-tool policy, probing, the
registry store and what each agent reaches; see its
[README](../../src/mcp/README.md).

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

   A bundle may also ship its servers as `companies/<name>/mcp.json`, in the
   `mcpServers` shape. It is read by tinymcp's `config_doc::parse_with` in
   lenient mode, with `endpoint`, `readOnlyTools`, `authSecret` and `$comment`
   registered as host fields ([`mcp::decl::file`](../../src/mcp/decl/file.rs)).
   An entry tinymcp refuses — an unknown field, a wrong type — is dropped and
   reported as a manifest problem without costing its siblings; an inline
   `headers` block or a query-string credential is refused because the file is
   committed.

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
stdio `command`. See [`mcp::decl`](../../src/mcp/decl/validate.rs).

## Credentials are write-only

A server's outbound token lives apart from its declaration, under the per-server
key `mcp/{name}/auth`. It is **write-only** over the API: set via the `token`
field on add/update, stored in the secret store, and **never** returned. The
read shape carries only an `authConfigured` boolean.

The agent-facing surface is redacted too: no company agent's tool scope names
`mcp_list_servers`, because OpenHuman's own implementation serializes each
server's credentials into agent-visible output. The persona brief names the
agent's granted servers instead — names only, no endpoint or auth. Regression
tests drive the native `mcp_call_tool` against an in-process MCP server and
assert the bearer reaches the *server* over the wire but never appears in any
`ToolResult`, including when the server reflects it into an error or a success.

## Per-agent scoping

An agent reaches a server named `<slug>` only when its manifest `tools` grants
match `mcp:<slug>` — the same glob semantics as every other tool grant
(`mcp:*` grants all). `resolve_for_agent` filters the resolved decls to the
enabled, granted set and attaches each to the agent's spec, with every tool the
agent's per-tool policy blocks on its deny list; `gitbooks.enabled = false` keeps
OpenHuman's default gitbooks server out. An agent with no granted MCP server has
no `mcp_list_tools` / `mcp_call_tool` in its tool scope.

```toml
[[agent]]
id = "researcher"
role = "Researcher"
tools = ["mcp:notion", "mcp:linear"]   # or "mcp:*"
```

Each `GET …/mcp/servers` row carries `accessGrant` and `agentAccess`: per agent
its `state` (`inherited`/`included`/`excluded`/`blocked` by the ceiling),
`reaches`, and the whole `tools` list that grants (`grantTools`) or withdraws
(`revokeTools`) it, `mcp:*` expanded host-side. The server page's **Agents with
access** editor sends that list via `PATCH …/team/{id}`; off the harness path
the edit rebuilds the runtime.

`mcp_call_tool` runs under a permissive OpenHuman `SecurityPolicy`. It is still
classified for audit, but policy-generated HITL is disabled.

## Approval behavior

`mcp_call_tool` does not automatically park under `supervised`. An agent that
needs sign-off calls `request_approval` explicitly before invoking it.
`readonly` remains a hard denial.

`mcp_list_tools` does not require approval. It changes nothing and is billed
for nothing. This matters more than one saved prompt: the persona brief
appended to every MCP-granted agent *instructs* it to answer capability
questions from a live `mcp_list_tools` call on a named server rather than from
memory. These reads must remain
uninterrupted so the guidance that prevents stale answers is usable on an
agent's first move.

The classifications remain declared in
[`policy::consequence`](../../src/policy/consequence.rs) for audit and for a
future policy-HITL mode.

## What a call reports back

Every `mcp_call_tool` result carries tinymcp's `McpCallOutcome` as host-only
metadata (`{kind: "mcp_call", server, tool, ok, error?}`), which OpenHuman
forwards on the turn's completed-call events. After each turn the agent's
`AgentMcpObserver` ([`mcp::observe`](../../src/mcp/observe.rs)) reads them:

- **`ok: true`** — the server answered, even if the remote tool returned its own
  error. One `OauthCall` usage sample is recorded under `mcp:<server>`.
- **`ok: false`** — the call failed before an answer: a 401, a transport error,
  a non-MCP reply, a JSON-RPC rejection. It is classified by
  `probe::classify_call_error` into the same status codes a probe uses
  (`credential_required`, `oauth_required`, `token_rejected`, `unreachable`, …),
  scrubbed against the server's credentials, and recorded on the company's
  `McpCallObserver`. The brain drains it onto the operator bubble as a red
  `MCP: <server> unavailable` step and journals a `McpCallFailed` event,
  stamped with the task id on a dispatched card.
- **Refusals** — a blocked tool (`ToolNotAllowed`) or malformed arguments — are
  neither metered nor surfaced; the agent reads the refusal in its result.

The registry bridge's `mcp_registry_tool_call` carries the same outcome (see
[Directory-installed MCP servers](mcp-registry.md)), so both bridges meter and
surface alike.

## Signing in with OAuth

`POST …/mcp/servers/{name}/oauth/start` and the unauthenticated
`/oauth/mcp/callback` run tinymcp's `OAuthFlow` with
`require_public_endpoints`: discovery, dynamic client registration, PKCE and the
code exchange, every discovery-supplied endpoint refused unless it is `https` on
a public address. One flow per host holds the parked sign-ins, keyed by
`state` under `<company>/<server>`, and sweeps abandoned ones after ten minutes.
The minted token is stored through `store_auth` as `AuthMaterial::OAuth`, and
the harness refreshes a near-expiry one through `OAuthFlow::refresh`, which
re-checks the stored token endpoint
([`company::mcp_oauth`](../../src/company/mcp_oauth.rs)).

The console then re-tests the server every 2 s (and on tab return) until `ok`,
toasts `Connected to <name> · N tools`, and after 5 minutes offers Check now.

## Per-tool permissions

Each server carries a policy document saying, per remote tool, whether a call
runs, parks for approval, or is refused outright. See
[Per-tool permissions](mcp-tool-permissions.md).

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
| `POST` | `…/mcp/registry/install` | Declare an entry as one of this company's servers (+ a write-only credential). |
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

`…/mcp/registry/install` writes the **same runtime index** `POST …/mcp/servers`
writes, so a server found in the directory is an ordinary `runtime`-sourced row
and the rest of the surface — conflicts, manifest override rules, probes,
credential rotation, delete — treats it exactly like one an admin typed in by
hand. It used to write OpenHuman's *separate* install store, through an RPC
that upstream has since removed: the directory there is browse-only now, and a
server found in it is declared by the reader rather than installed by a
catalogue action. Declaring it here, from the entry the console already
fetched, is what saves the operator retyping an endpoint they are looking at.

The credential is supplied the way `POST …/mcp/servers` supplies one — a
`token` plus an `authKind` of `bearer`, `header` or `queryParam` — not as the
`env` map the removed RPC took. An entry's `requiredEnvKeys` names a launcher's
environment, and a hosted HTTPS endpoint has no launcher, so how the secret
reaches the server is a question to answer rather than guess from a key's
spelling. The console shows those keys beside the field as guidance.

The other registry routes still address OpenHuman's install store, which still
holds anything installed there before this change.

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

A company can also **install** a server from the MCP directory rather than
declaring one. Those installs, how they reconcile with declared servers, the
explicit grant that reaches them and the per-install scoping under it live in
[Directory-installed MCP servers](mcp-registry.md).

## Which builds can honour a server (issue #567)

The management routes above are **ungated** — they ship in every build. The
agent-side bridge is not: servers are attached to a teammate's spec only behind
`#[cfg(feature = "mcp")]`. Three configurations, only one of which the
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
`frontend/src/api/mcp-registry.ts` (the directory) — rendered inline on
Connections and as Settings, MCP Servers
([`McpServersView`](../../frontend/src/views/McpServersView.tsx)).

The page has two modes on one switch
([`mcp-view-controls.tsx`](../../frontend/src/views/connections/mcp-view-controls.tsx)),
kept in the hash as `?view=yours|discover`:

- **Yours** searches this company's own servers and nothing else; typing never
  reaches the directory. A term that matches nothing offers to search the
  directory for it, which carries the term into Discover.
- **Discover** is the public directory, with its own field. Before anything is
  typed it shows the top connectors (see
  [mcp-registry.md](mcp-registry.md#ranking-and-top-connectors)); "Browse the
  directory" on an empty company opens it, and a company with no servers opens
  on it.

Each mode has a card / list toggle, remembered per mode in `localStorage`
(`opencompany.mcp.layout.<mode>`). Yours defaults to a list and Discover to
cards. The list is one table shape for both: name and icon, source (`md` and
up), status, reach (`lg` and up), and the actions. The name column takes
whatever width the others leave and truncates; the page does not scroll
sideways at phone width.

A row has no expander. Clicking anywhere on it that is not a control opens the
server's page (`?server=<name>`); the primary action (sign in, add a token,
connect) and an overflow menu holding enable/disable, re-check, tools,
permissions and remove are the only controls on it. A double-click or a drag
opens the page and selects nothing.

**mcp.json** ([`McpJsonEditor`](../../frontend/src/views/mcp/McpJsonEditor.tsx))
is a button and a pop-up over `…/mcp/config`, opened by `?tab=json` as well, and
read-only for a member. It is the same store the rows read: a save re-reads the
server list (`onSaved` → `refresh()`), so the rows never describe the
configuration as it was before the file was written.

**Add custom server**
([`McpAddServerDialog`](../../frontend/src/views/connections/McpAddServerDialog.tsx))
asks for a name and an MCP URL only. A server that answers its probe is
confirmed in place ("Added and connected · N tools"); one that needs a sign-in,
a token or env credentials continues in the connect dialog.

The **connect dialog**
([`McpConnectDialog`](../../frontend/src/views/mcp/McpConnectDialog.tsx)) is
where every connect flow lands — a custom add, a directory install, a connect
from the overflow, a sign-in. It carries the sign-in in flight, the token and
env forms, and the tools list, and ends on "Connected · N tools" rather than a
toast that has gone by the time the operator looks for it.

There is deliberately no MCP method on `OpenCompanyClient`. A second set used to
sit there, declaring a `{ servers }` wrapper around this table's bare array,
`server_id` keys, and `/connect` / `/disconnect` routes that exist nowhere; the
Settings page built on it crashed on open (issue #414). The client casts an
unparsed body to the declared type, so a second surface is never caught by the
compiler — only by whoever opens the page.

### Browsing the directory

[`McpRegistryBrowser`](../../frontend/src/views/connections/McpRegistryBrowser.tsx)
renders Discover under the same manage gate as adding a server (issue #403 — an
install hands every teammate a new set of tools). An entry opens a pop-up with
its endpoint, publisher and verified badge before anything is installed; an
entry this company already holds says so and offers no install. What it
installs lands in Yours with a `registry` badge.

An entry's install form is exactly the `requiredEnvKeys` the host derived from
the connection the install will use, as password fields. Those values are
write-only in both directions: nothing sends one back, and the merged row
reports only `authConfigured`.

Its failures are its own. The directory is a network hop and can be down, and on
a build without the `mcp` feature every `…/mcp/registry/…` route answers
`404 not_wired` — so `registryOutage` in `frontend/src/lib/mcp-registry.ts`
turns *every* rejection into one of two notices and never rethrows. A dead
directory is an empty result with a reason; a missing feature is a sentence
about the build. The company's installed servers keep rendering through both.
Slow or failing reads: [mcp-registry.md](mcp-registry.md#a-slow-or-failing-directory).

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

A server opens into its own page
([`McpServerPage`](../../frontend/src/views/mcp/McpServerPage.tsx)), laid out
after a connector page: icon, name, provenance and standing in the header with
the primary action and disconnect beside them; then its description, tools and
permissions, the agents that can reach it, and usage. Connection facts sit
behind a **details** pop-up rather than at the foot of the page. Its provenance
and removal prose are `mcpProvenanceNote` / `mcpRemovalNote` — one sentence per
source.

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

## What a server says about itself

`initialize` carries a `serverInfo` block the protocol leaves open-ended, and a
server may put a `title`, a `description`, a `websiteUrl` and an `icons` array in
it. The probe reads it off the transport's cached handshake — the listing already
performed one, so this costs no extra round trip — and keeps it at
`mcp/{name}/server_info`, beside the health record. Coverage is patchy in
practice (Context7 answers with all four, DeepWiki with none), so every field is
optional and absent stays absent: a placeholder would be this host asserting
something the server never said. A failed probe leaves the previous record
standing, as it does the inventory.

The read carries them as `probedTitle`, `probedDescription`, `websiteUrl` and
`iconUrl`. `probedDescription` is separate from `description`, which is what the
operator or the bundle declared — the console offers the server's own words as
the default for that field rather than overwriting a declaration with them.

**An icon is fetched by the host, never linked.** The URL is chosen by whoever
runs the remote server, and in an `src=` it is a beacon that fires for every
operator who opens the Connections page and reports to that host who looked and
when. So the bytes are fetched during the probe behind the outbound SSRF guard
with redirects off, capped by reading the body rather than trusting a declared
length, typed from their own signature rather than the claimed `Content-Type`
(which is how an SVG — a document that can carry script — is refused whatever it
was labelled), held to the avatar decompression-bomb check, and stored inline as
a `data:` URI. Rendering one therefore reaches nothing. A fetch that fails leaves
`iconUrl` absent and the console draws its letter tile. The stored value is
re-checked on read, so a tampered store cannot turn the field back into a remote
request. Same reasoning as the avatar grammar in
[`src/company/avatar.rs`](../../src/company/avatar.rs).

## When a config change reaches an agent

An agent materializes its MCP registry when the
[`HarnessPool`](../../src/harness/mod.rs) builds a company's roster, but the
pool re-checks that registry on the way into every turn. `ensure_with_policy`
re-resolves the effective server set, hashes it with `mcp_fingerprint`, and
rebuilds the roster when the hash moved against the cached `mcp_fingerprints`
entry. A mid-session edit — add, disable, token rotation, a per-tool permission
change — therefore reaches a live agent on its **next turn**, with no company
restart. Every mutating API response says as much (`NEXT_TURN_NOTE` in
`src/server/ops/mcp.rs`).

The check is a store read plus a hash, not a rebuild, so it costs the same
whether or not anything moved. What it does not reach is a turn already in
flight: an agent mid-turn finishes on the belt it started with, because the
fingerprint is compared before the turn, not during it.

## `read_only_tools` and where this is headed

The current flat allowlist is planned to become a per-tool, three-tier
permission model (Interactive / Read-only / Write-delete, with bulk defaults
and per-tool overrides). The design brief and rollout order are in
[`docs/issues/mcp-refactoring-enhancing/`](../issues/mcp-refactoring-enhancing/README.md)
(tracking issue #2373). `read_only_tools` remains the migration input; this
brief does not change runtime behavior.
