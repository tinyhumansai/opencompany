# The desktop client

OpenCompany ships as an orchestration **server** and a **desktop client**. The
console stays one codebase and the server stays one binary; what the split adds
are the seams a desktop needs — several hosts at once, a credential a webview
can carry, and a host running in-process.

Code: `src-tauri/` (a separate crate, not a workspace member) and
`frontend/src/connections/`.

## Why `src-tauri` is not a workspace member

Making it one would put the whole Tauri tree into `cargo metadata --locked`,
which CI runs first on a runner with no webkit or GTK — turning a desktop-only
dependency into a hard requirement for checking the server. The host stays a
plain `path` dependency, so a change to it is picked up with no publishing step.

The cost is that no root `cargo` invocation reaches the desktop, including
`--all-features`. The `Desktop` job in `.github/workflows/ci.yml` is what builds
and tests it; without that lane the crate would be compiled by nothing, which is
[issue #475](https://github.com/tinyhumansai/opencompany/issues/475)'s shape.

### One Tauri app, because two made the desktop unusable

There used to be two. `frontend/src-tauri/` was a second Tauri crate sharing
this one's `productName`, and which one a `tauri` invocation picked up was
decided by the working directory in a way most people do not expect: **the CLI
searches subfolders of the working directory, not ancestors.** From `frontend/`
it found the wrapper; from the repository root or from `src-tauri/` it finds
this one.

`tauri:dev` and `tauri:build` are scripts in `frontend/package.json`, and npm
runs a script from its manifest's own directory — so `npm run tauri:dev`, the
obvious way to start the desktop app, started the wrapper every time. The
wrapper registered exactly one command, `desktop_config`, and the console had
stopped invoking it (see the comment at the top of `frontend/src/main.tsx`):
every `oc_*` command the console uses to reach a host — `oc_embedded` first —
was absent from its `generate_handler!`. The window opened, the console
rendered, and no host ever appeared. The symptom reads as "the server doesn't
start"; the cause is that the app which starts one was never the app that ran.

Nothing could see it. Both crates compiled, both were tested by the `Desktop`
lane, and the packaging steps ran from the two directories that find the shell.
A second app is not a hazard that becomes safe by being currently correct — the
ambiguity is the hazard — so the wrapper is deleted, `frontend/package.json`
points `tauri:dev` at `scripts/desktop-dev.sh` and `tauri:build` at
`src-tauri/`, and `scripts/ci/assert-single-tauri-app.sh` fails the build on
either a second `tauri.conf.json` or a script that invokes a bare `tauri`.

## What the desktop compiles in

The desktop links the host with an explicit feature set, and it is declared in
**two** places — reading only the first is issue #1738. The manifest's list, on
the `opencompany` dependency in `src-tauri/Cargo.toml`:

```toml
opencompany = { path = "..", default-features = false, features = [
  "sqlite", "platform-jwt", "oauth", "mcp", "tinymemory",
] }
```

and the shipped set, passed on the `tauri` command line as
`DESKTOP_RELEASE_FEATURES` in `.github/workflows/release-desktop-macos.yml`:

```text
opencompany/acp,opencompany/composio
```

Those two are `= ["openhuman"]` in the root manifest — pure `cfg` switches that
pull no `dep:` entry, and `mcp` already enables `openhuman` — so turning them on
adds no package and leaves `src-tauri/Cargo.lock` byte-identical, which is what
lets a release still build `--locked`. A feature gating an optional dependency
could not be passed this way and would have to move into the manifest.

The cost of that arrangement is several copies of one string, and #1738 is what
a missing copy looks like: `scripts/desktop-dev.sh` ran `tauri dev` bare, so a
developer's shell was the manifest set and every DMG was the release set. The
visible half was Connections — `in_build: cfg!(feature = "composio")` reported
false, so eight provider tiles rendered "not available here" over a card asking
the operator to paste a Composio token, on a build nobody ships. `acp` was dark
the same way and less visibly (`acp_agents = None`, so every `transport =
"local"` harness resolved `unavailable`; issue #1245).

#1823 fixed the launcher without adding a copy: it **parses**
`DESKTOP_RELEASE_FEATURES` out of the release workflow, with a
`DESKTOP_FEATURES` override for the leaner build. A derived value cannot drift.

`scripts/ci/assert-desktop-features.sh` guards what is still duplicated (the two
`ci.yml` steps, `npm run tauri:build`, the by-hand command below), that the
release `tauri build` still consumes the variable it declares — otherwise the
source of truth is a lie — and that the launcher still derives rather than
re-hardcoding a literal.

`mcp` is the one that puts an agent harness in the app. It implies `openhuman`,
which is what compiles `src/harness/` at all; without it the bundle boots, seeds
a company, serves the console — and cannot think. The visible symptom was the
setup wizard's inference test answering *"This build cannot reach a model — the
agent harness is not compiled in."* for every provider, however good the key.

The belt a desktop agent gets is deliberately the minimal one. The host declares
`openhuman_core` with `default-features = false, features = ["skills", "mcp",
"hosting"]`, so what a company can use is **built-in tools, MCP servers and
skills** — no memory engine, no TokenJuice, no voice or inference stack out of
the vendored runtime. Features left off, each on purpose:

| Off | Why |
| --- | --- |
| `tinycortex`, `tinymemory*` | In-pod memory engines. They carry tinycortex, `tinyagents/sqlite` and a second bundled SQLite into the bundle for a surface the desktop does not offer; the runtime keeps its fs-backed memory stores. |
| `media` | Unlike `composio` it is `["openhuman", "openhuman_core/media"]`, so it pulls an upstream domain nothing else here compiles, and its credential really is managed: the tools are wired only when a company grants the namespace **and** a platform credential is configured. There is no BYO tier, so no desktop operator has anything to supply, and `…/capabilities` answers `media_in_build: false`. |
| `mongodb` | A per-tenant cluster is a hosting concern. |

`composio` and `acp` are **not** in this table. They are shipped, by the command
line above. The row that used to exclude them said the managed backends "need a
platform credential the desktop has no way to hold", and that was never true of
`composio`: `company::composio::resolve_credential` answers over three tiers and
the platform identity is the *last* — the BYO `composio/token` override wins,
then the company's own TinyHumans key. Tier one is exactly what a desktop
operator can hold, and the Connections card already asks them for it. It also
named a `search` feature, which does not exist; `search_in_build` derives from
`cfg!(feature = "openhuman")`, which `mcp` already turns on.

### The `[patch]` table is replicated, not inherited

A `[patch]` section only applies in the workspace root that declares it, and
this crate is its own workspace. Until `mcp` put `openhuman_core` in the graph
none of the vendored crates were reachable from here, so the table could be
omitted. Now it cannot: without it Cargo resolves `tinycortex-api`, `tinyflows`
and the rest from crates.io — where some do not exist at all — and any that did
resolve would be a *second* copy whose trait identities would not match the ones
the host compiled against. `src-tauri/Cargo.toml` therefore carries a replica of
the host's table with every path prefixed `../`. Keep the two in step.

### Attaching the harness is the library's job

Compiling the harness in is half of it; something has to hand each company a
pool. That sequence — the pool, plus whichever managed media/search/inference
backends the environment supplies — lives in `opencompany::app::attach_harness`
and is called by both `serve` and `desktop::register`. It used to be a private
function in `src/bin/opencompany.rs`, which is precisely why the desktop path
built companies with no harness even once the feature was on.

`embedded::start_with` additionally pins the vendored keyring to the instance
root and installs the product identity before any runtime exists — the same two
startup calls `serve` makes, and for the same ordering reasons (see
`src/app/journal.rs` and `src/product.rs`).

## Packaging is a claim the lane has to make

Compiling and packaging are different claims. `cargo fmt`, `cargo clippy` and
`cargo test` drive `cargo` directly; none of them reads `tauri.conf.json`, so a
lane built from those three can be green over an app that cannot be assembled at
all. That is what happened: `beforeBuildCommand` named a path that escaped the
repository, `cargo tauri build` and `cargo tauri dev` failed on their first step
for every developer, and the `Desktop` lane never noticed because it builds the
console itself with `working-directory: frontend` and then calls `cargo`.

The `Package` steps close that. They run the real CLI —
`tauri build --debug --no-bundle` — so the config is executed rather than merely
committed. `--debug` because the `Test` step already compiled that graph in the
dev profile and a release build would recompile the host for no extra claim;
`--no-bundle` because the failure being gated happens at the first step of
`tauri build`, long before a `.deb` exists.

There are two of them, from the repository root and from `src-tauri/`. Every
other step in this lane runs from the repository root, which is the one place
the broken hook happened to work — a single-directory packaging step is how #616
stayed invisible. Nothing working-directory-dependent survives in the config
today, so what the pair defends now is that none comes back.

### Build the console first: there is no `beforeBuildCommand`

Both hooks are empty, and that is deliberate. **Build `frontend/dist` before you
package**:

```sh
npm --prefix frontend run build     # from the repository root
cargo tauri build -- --features opencompany/acp,opencompany/composio
```

The `--features` is not optional decoration. `tauri build` without it packages
the **default** set, so a locally-packaged app has Composio and ACP compiled out
while looking in every other respect like the shipped one — #1738 at the
packaging entry point, and harder to spot there than in a dev window. `npm run
tauri:build` carries the same string, and
`scripts/ci/assert-desktop-features.sh` fails if the two drift from
`DESKTOP_RELEASE_FEATURES`.

`frontendDist` is resolved relative to `src-tauri/`, where `tauri.conf.json`
lives, so it means the same thing from every working directory. A hook does not:
Tauri runs it from an app directory it *derives*, by scanning for a
`package.json`, and which one it finds is not stable across machines. The
committed `../frontend` escaped the repository entirely from `src-tauri/`
([#616](https://github.com/tinyhumansai/opencompany/issues/616)), and the
opposite prefix fails from the repository root — each is correct in exactly the
directory that hides the other:

| hook value    | from repo root | from `src-tauri/` |
| ------------- | -------------- | ----------------- |
| `../frontend` | passes         | **fails** — what shipped |
| `frontend`    | **fails**      | passes            |

Resolving the path inside the hook does not rescue it either. `$(git rev-parse
--show-toplevel)/frontend` passes from both of those, and still broke in CI: the
hook landed in `vendor/openhuman/` — another directory with a `package.json`,
reached first because a Linux runner enumerates directories in a different order
than a developer's macOS checkout — and `git rev-parse` inside a submodule
answers with the *submodule's* root. The CLI offers no flag, config key or
environment variable naming the app directory, so nothing computed from the
working directory can be trusted.

Deleting the hook removes the whole class. The cost is that `tauri dev` no longer
starts Vite for you, which is what `scripts/desktop-dev.sh` is for: it brings the
console up on `localhost:5173` — reusing one already there, never killing
somebody else's — waits until that port answers with the console rather than
with a stranger's page, and then runs `tauri dev` from `src-tauri/`.
`npm run tauri:dev` in `frontend/` is that script. Driving `tauri dev` by hand
instead means running `npm --prefix frontend run dev` alongside it; `devUrl`
already points at `localhost:5173`. The other cost is that packaging a stale
console is now possible locally, where before it was merely likely. The failure mode is at least
legible: Tauri reports `Unable to find your web assets … frontendDist is set to
"../frontend/dist"` with the absolute path it resolved, rather than an `npm
ENOENT` for a directory nobody named.

Which kinds of host it can hold, and how an operator picks one, is
[`connectors.md`](connectors.md).

## N connections, and no active one

`frontend/src/connections/registry.ts` holds a map of connections and
deliberately has **no "active connection"** field. Selecting a host in the UI is
a rendering choice, not a state change: every connection stays registered and
probed, so one host being unreachable reddens one row rather than blanking the
app.

That single-valued field is what stops comparable clients from holding more than
one host at a time, and it would be just as limiting above the seam as below it.
The Rust `ProxyRegistry` has the same shape for the same reason, and every
command takes an explicit `connection_id`.

Every browser-local key is namespaced by `(connection, company)` through
`scopedKey`. Company alone is wrong as soon as two hosts serve a company of the
same name; connection alone is wrong as soon as one host serves two companies.
Anything reading or writing that state must depend on **both** — a callback that
closes over the scope but depends only on the company will write under the host
the operator just switched away from.

### On the desktop a base url is absolute or it is nothing

A browser can be given anything, including the empty string, which means *same
origin* — that is how every web deployment finds its host, since
`opencompany serve` mounts the console at the origin serving the assets.

The desktop is the runtime with a rule. `ProxyTransport` hands the base url to
Rust, which joins it to a path by concatenation, so anything without an
authority yields a *relative* url and `reqwest` refuses it at `send`. The
request never reaches a socket, and the console reports "couldn't reach a
company host" about a host that was never addressed.
`isAddressableBaseUrl()` is the one place that says so — both the bootstrap add
in `App.tsx` and `restoreConnections()` ask it, and `ProxyRegistry::upsert`
enforces the same thing from below, at the last moment the caller is still on
the stack.

The empty string is the form
[#613](https://github.com/tinyhumansai/opencompany/issues/613) reported, and
only the shortest one: `/api` and `localhost:8080` fail identically, and the
second is what someone types into "Add a host". Parsing is not enough either —
`URL` accepts `tauri://localhost` and `file:///x`, and neither is a company
host — so the check is `http:` or `https:`.

Whether that host may then be *trusted with a secret* is a separate question,
answered separately — see "Where a credential may travel" below. Collapsing the
two would either forbid anonymous HTTP or permit credentialed HTTP.

Two consequences follow, and both are load-bearing
([#613](https://github.com/tinyhumansai/opencompany/issues/613)):

- **The desktop can hold zero connections.** The embedded host arrives over IPC
  and may never arrive at all. The rail therefore stays on screen at a count of
  zero — it holds the only "add a host" there is — and the console renders the
  absence rather than an empty pane.
- **Launch selection is stated, not sorted.** Restored hosts are added before
  the embedded one, so list order records when a host was learned about, not
  which one a person means. `App` selects the embedded host when nothing has
  been chosen.

Only the same-origin *default* is refused. A desktop pointed at a real host
through `?api=` or an injected `OPENCOMPANY_CONFIG` still gets its bootstrap
connection.

## The transport seam

`Transport` has two implementations, chosen at runtime by `isDesktopRuntime()`
so `frontend/` stays one build artifact:

- `BrowserTransport` — `fetch` and `EventSource`, a literal restatement of what
  the console did inline before. The browser build's behaviour is unchanged.
- `ProxyTransport` — every request and event stream through the app's own Rust
  core.

The desktop routes through Rust for three reasons, in the order they bite:

1. **CORS.** A webview origin is cross-origin with every host, so a direct fetch
   would need each operator to allow-list `tauri://localhost` before their
   desktop could connect — configuration standing in front of the headline
   feature. Requests made from Rust are not subject to CORS.
2. **The credential.** A device token attached in Rust never enters the webview.
3. **Streaming.** `EventSource` cannot set a request header, so it cannot carry
   the session header, and a `SameSite=Lax` cookie is never sent cross-site.

`src-tauri/tests/proxy_parity.rs` runs both transports against one real host and
compares, because the console's error handling reads the status, the body and a
response header — a transport that differed in any of them would produce
different `ApiError`s on the desktop for the same server behaviour.

### One reader of `window.__TAURI__`

`app.withGlobalTauri` assigns that global the whole `@tauri-apps/api` bundle, and
**v2 namespaces it by module**: the keys are `app`, `core`, `dpi`, `event`,
`image`, `menu`, `path`, `tray`, `webview`, `webviewWindow` and `window`, and
`invoke` and `Channel` are under `core`. The bare `__TAURI__.invoke` is the v1
shape and reads `undefined`.

`frontend/src/api/transport/bridge.ts` is the only file that touches the global.
Before [#616](https://github.com/tinyhumansai/opencompany/issues/616) two
transports read it separately and both read the v1 shape, so `bridge()` resolved
to `null`, `oc_connect` never ran, no connection was registered and the console
reported an unreachable host — a network-shaped symptom for a bug that never
opened a socket.

The unit tests could not catch it, because they asserted the same wrong shape:
every mock hand-wrote `{ invoke, Channel }` at the top level, and 82 desktop
tests passed against a fixture the runtime never produces. So
`test/unit/desktop-bridge.test.ts` now reads the shape off `@tauri-apps/api`
itself and asserts the v1 form is **refused** — a mock is evidence only if
something ties it to the real thing.

`isDesktopRuntime()` still probes for presence alone, deliberately: a `__TAURI__`
whose `core.invoke` does not resolve is a broken desktop rather than a browser,
and `ProxyTransport` throwing "the desktop bridge is unavailable" names that,
where falling back to `BrowserTransport` would bury it in a CORS failure against
every host.

### Registration precedes traffic

The core resolves a connection id against its own registry, so the console must
call `oc_connect` before any request for that id. `addConnection` is synchronous
(React renders off it) and `oc_connect` is not, so each registration is kept as
a promise and awaited by `ProxyTransport` before its first call. Firing and
forgetting loses the race on a fast probe, and the symptom — a host that is
unreachable once and fine on retry — reads like a network fault.

### What the proxy will not carry

Caller-supplied `x-opencompany-session`, `authorization`, `cookie` and
`proxy-authorization` headers are dropped before the connection's own credential
is attached. `RequestBuilder::header` appends and axum reads the *first* value,
so a header from the webview would otherwise be the one the host honoured.
Keeping the token out of the webview is worth little if the webview still
decides what a request authenticates as.

### Where a credential may travel

Addressability is not the only question a base url has to answer. A host can be
perfectly reachable over a wire anyone on the path can read, and the desktop's
credential is a device session — a person's standing authority on a company,
attached to every request and to the whole life of the event stream, and
replayable by whoever copies it down
([#731](https://github.com/tinyhumansai/opencompany/issues/731)).

So a second rule sits beside the first: **a credential travels over HTTPS, or to
a host on this machine, and nowhere else.** `may_carry_a_credential` in
`src-tauri/src/proxy/mod.rs` is the one that enforces it, with
`mayCarryACredential` in `frontend/src/api/transport/index.ts` as the console's
copy — the same arrangement as `isAddressableBaseUrl`, and for the same reason:
a check in the console alone is bypassed by anything reaching the proxy
directly, and a check in Rust alone arrives as an opaque IPC rejection that
`client.ts` flattens into "cannot reach the company host".

Loopback is exempt because `http://127.0.0.1:<port>` is how the embedded host is
reached, on a port that changes every launch and so can never carry a
certificate; `localhost` and its subdomains come with it, per RFC 6761. The
private ranges are deliberately **not** exempt — an office LAN is precisely
where someone else is on the path.

The rule turns on the credential rather than on the scheme, which is what keeps
a home-lab or staging host without a certificate usable: an anonymous connection
to one still registers and still reads, because nothing is exposed that a
passer-by could not have asked the host for themselves. Three surfaces apply it:

- `ProxyRegistry::upsert` refuses to register a credentialed connection to such
  a host, by name — `this host is not encrypted`, not `not an absolute host url`,
  because an operator told the second goes to debug a network that is working.
- `claim` in `commands.rs` refuses the pairing exchange before opening a socket.
  This is the one place a session token is *created* rather than replayed — the
  code goes out in the request and the token comes back in the response — and it
  never touches the registry, so `upsert`'s refusal does not cover it. Its client
  also refuses redirects: a 307 from an HTTPS base to an HTTP one would re-send
  the claim, body and all, over the wire the check just refused.
- `probe` in `registry.ts` marks such a connection `down` with the reason,
  before contacting it, so the row says what is wrong instead of blaming the
  network.

The webview also runs under a CSP (`src-tauri/tauri.conf.json`) whose
`connect-src` allows the IPC origin only. All host traffic goes through Rust and
needs nothing else.

## The embedded host

`src-tauri/src/embedded.rs` runs a real host in-process on `127.0.0.1:0`,
holding the data root's lock (see [`data-root.md`](data-root.md)). It becomes an
ordinary connection in the console, discovered through `oc_embedded` because
only the core knows which port the OS chose.

That root is the same canonical data directory as the CLI: `$HOME/.opencompany`
(or `%USERPROFILE%\.opencompany` on Windows). `default_data_dir` in
`src-tauri/src/lib.rs` delegates to the host resolver and passes the result
explicitly to `app::prepare_instance`. `OPENCOMPANY_DATA_DIR` overrides it for
both launchers. See [the desktop root](data-root.md#the-desktop-root-is-the-cli-root).

Loopback and never `0.0.0.0`: an embedded instance is this machine's, and
binding a routable address would publish someone's company to their network.

It binds a real socket rather than driving the `Router` in-process through
`tower::Service`. Skipping the socket would save microseconds and buy a second
code path through the auth extractors, the CORS branch and the error envelopes —
divergence in an auth extractor being precisely the class of bug that cannot be
afforded. With a real listener, every Playwright spec and every proxy test is
valid evidence about embedded mode too.

Not started when its root could not be taken — most often because another
process holds it. The console renders that as a row; the desktop still holds
remote hosts, which is the point of holding several.

### No sign-in at all

The embedded host sets `auth_mode_override` to `AuthMode::None`
([sign-in modes](auth-modes.md#none)). A desktop install has no login screen,
no operator mailbox, and no session: `resolve_principal` answers with the
company's implicit local owner before it looks for a cookie or a bearer, so the
console's first request is already authenticated as an `Admin` backed by a real
`UserRecord` under `local:owner`.

The argument for it is not that the login screen was redundant. It is that
**there was never a session carrier to bring its result home in.** The magic
link worked — a loopback host with no mail transport echoes the code in its own
response, and the console redeemed it — and then the cookie went nowhere:

- The proxy's `reqwest` client is built without a cookie store, so nothing
  persisted the `Set-Cookie` for the next request.
- `x-opencompany-session`, the header carrier a paired device uses, is in
  `RESERVED_HEADERS` and is stripped from anything the webview sends — see
  [what the proxy will not carry](#what-the-proxy-will-not-carry).
- `needsCarriedSession()` is false in the desktop whatever the address, so the
  console does not hold a token either.

What actually let the console through was that every request came from loopback
anyway, which is `none` mode's premise stated in a slower way. So the ceremony —
a synthetic `operator@opencompany.local` the operator was told to accept, a link
the host mailed to nobody, a cookie discarded on arrival — bought nothing that
the bind was not already providing. `none` deletes it and says what was true.

Set on the **host**, not in the shipped preset manifests: the override reaches
every company on the data root, including ones an earlier install left there,
so an existing install migrates by relaunching — and it leaves
`manifest.users.mode` at its default, which a `[users].admins` entry under
`mode = "none"` would otherwise make `validate_users` reject.

A default, not a ceiling. `prepare_instance` reports the root's `config.toml`
`auth_mode` and `none` is only the fallback, so an operator who deliberately
turns a sign-in on in setup — to share their instance — still has it after a
relaunch. The setup wizard preselects `none` when it is running in the desktop
runtime *and* the host offers the mode, which is a preselection and not a lock.

**What this costs:** a device paired to the embedded host from another machine
stops working. Every step of the pairing succeeds and the resulting token is
inert from anywhere but this computer; see
[sign-in modes](auth-modes.md#none) for why, and note that pairing this desktop
*to a remote host* is unaffected — that is the section below.

## Several hosts on one machine

Moved to [desktop-hosts.md](desktop-hosts.md): how the roster of local hosts
is stored and reconciled, and why instance_id rather than address is the
identity rule.

## Authenticating as a person

Moved to [desktop-hosts.md](desktop-hosts.md): device credentials, backend
selection, and the Linux session-keyring caveat.

## ACP

Moved to [desktop-hosts.md](desktop-hosts.md): the stdio harness client and
the reserved /acp prefix.
