# The company credential

How a company proves who it is to the surfaces the platform brokers on its
behalf (issue #586). The routes are listed in
[`api-write-plane-credentials.md`](api-write-plane-credentials.md); this is the model
behind them.

## One key per company

A company belongs to one owner. Its admin sets **one** TinyHumans key through
`PUT …/credential`, and everything the platform backend brokers for the company
rides it. Membership in the company is what grants access to it.

Composio is the first consumer, and it is the one that shows why this works: the
backend derives the Composio entity from whatever bearer it is handed, and a
TinyHumans key is a bearer it recognises. So the key **authorizes Composio
directly** — there is no provisioning step that trades it for a second token,
and no per-tenant provider application to register with Google, Slack or GitHub.

### Not every TinyHumans key reaches Composio

The bearer has to carry the `connections` scope. The backend requires it on
*every* `/agent-integrations/composio` request, safe methods included, and
refuses to mint it on `POST /api-keys` — so a key a person created by hand in
their TinyHumans account, and then pasted into `PUT …/credential`, connects
nothing. It is a real credential that authenticates fine and is refused at that
one surface.

Two principals do carry it:

- **An attested hosted tenant.** The manager-issued, audience-bound cluster
  token the instance authenticates with is granted
  `read`/`connections`/`inference` automatically. A hosted company therefore
  reaches Composio with no key set at all.
- **A key issued by the grant flow below**, when the console that asked for it
  is a provisioned tenant origin.

This is worth stating plainly because the console cannot tell the difference
from the outside: `PUT …/credential` accepts any string, and a pasted key
reports `configured: true` and `source: company` whether or not it can do the
thing the card describes.

## Getting the key without pasting one

`POST …/credential/link/start` → the hub → `POST …/credential/link/finish`.
Modelled on OpenRouter's PKCE key exchange, and it exists because the
alternative was an operator leaving the app to mint a key by hand and carrying
it back on a clipboard.

1. **Start.** The host mints a `code_verifier`, keeps it in memory
   (`server::hub_link`), and answers with the hub URL to navigate to — carrying
   only `base64url(sha256(verifier))` and an opaque `state`.
2. **The hub.** The person signs in with their provider and approves a consent
   screen naming the requesting origin and the scopes.

   The URL is the **site's** `/connect` page where a site is derivable, and the
   API's `GET /auth/key` where it is not. `/auth/key` defaults to
   `provider=google` and redirects there immediately: an admin who pressed a
   button in their own console arrived at a Google account picker naming nobody,
   with no way to use the account they actually sign in here with. `/connect`
   names the instance asking, says what will be created, offers the same three
   providers the sign-in screen does, and hands off to `/auth/key?provider=…`
   with every grant parameter passed through — `server::hub_identity::key_grant_query`
   builds them once, so the two pages cannot disagree about the challenge. The hub decides those
   scopes from the callback origin: a **provisioned tenant origin** may receive
   `connections`; a loopback console receives what a human could mint by hand.
3. **Finish.** The browser returns with a single-use `code`. The host looks up
   the verifier by `state`, redeems both at the hub, and stores what comes back.

The verifier never leaves the host and the key never reaches the browser. That
is the reason the exchange is server-side rather than done in the page: whatever
redeems the code receives the key, and a `connections` key passing through a tab
is a credential in a place nobody can account for.

**Where the browser comes back to.** The callback is the query
`?company=…&key=link&state=…` appended to a base resolved in this order
(`server::ops::company_key::callback_base`):

1. **A stated `OPENCOMPANY_PUBLIC_URL`** always wins — `{public_url}/`, the
   console's own origin, because the console is what holds the session that
   may call `finish` and what redeems the code.
2. **A loopback `Origin` request header**, when nothing is stated —
   `{origin}/`, `http://` to `localhost` or a loopback literal only, the same
   shape the hub's own gate admits. This is what makes local development need
   no configuration at all: the dev console on `http://localhost:5173` is sent
   back to itself, because whatever pressed the button is where the answer
   should come back to. A non-loopback origin is not trusted here even though a
   stolen code redeems nothing without the verifier this host keeps.
3. **The host's own return route** otherwise —
   `http://{bind}/auth/key/callback` (`server::hub_link_callback`). No stated
   origin and no browser origin is the desktop: its console is a webview whose
   requests reach the embedded host through the shell's Rust proxy, so there
   is no `Origin`, and the host serves nothing at `/`. Sending the browser to
   `http://{bind}/` there was a 404 holding a spent code — and
   `http://127.0.0.1:0/` before the shell recorded the port it actually bound,
   which Chrome refuses outright. On this route the host redeems the code
   itself, through the same `redeem_link` the console's `finish` runs, and
   shows the tab a page saying to go back to the app. The route is mounted
   without console auth, like the MCP OAuth callback: the parked single-use
   `state`, bound to one company and expiring with the link, is the whole of
   its authority. The hub admits any `http` loopback URL, path included.

A host that advertises a stated origin serving no console still answers the
return leg with a 404 and the grant dies holding a spent code. In a hosted
tenant the console is served from that origin already; locally, either rely on
tier 2 automatically, or set `OPENCOMPANY_CONSOLE_DIR` to a built
`frontend/dist` so the host origin serves it.

**The key never reaches the browser.** The return leg carries `state` and a
one-time `code`, and nothing else — the console posts both to its own host,
which redeems them and stores the key. There is deliberately no screen anywhere
in this flow that displays the key: whatever holds the code and the verifier can
mint it, and a `connections`-scoped credential rendered into a page is one that
has passed through a tab, its history, and any extension reading either. A key
somebody wants to see with their own eyes is minted by hand on the dashboard's
API-keys page instead.

**One grant arms two credentials.** The minted key is stored as both
`tinyhumans/key` and `inference/key`, and the company's inference provider is
declared `managed`. An admin who had to run the flow once per page — once for
Connections, once for Inference — would be back to two errands, which is the
thing this replaces.

The paste field stays. A host with no hub wired reports `hubLink: false` on
`GET …/credential`, the console renders no button, and the screen is exactly
what it was before this existed.

### Which hub, and the two pages the console does not reimplement

Everything above happens against whichever hub `TINYHUMANS_API_URL` (or
`config.toml`'s `api_url`) names — the production one by default,
`https://staging-api.tinyhumans.ai` for a console working against staging.
Nothing else has to be set to move the flow: the authorize URL is built from
that value (`server::hub_identity::key_grant_url`), so is the callback, so is
the managed inference endpoint every company on the host resolves
(`docs/modules/inference/data-model.md`, "per process, at the floor"), and so
are the console's own "Get an API key" links — the Account dialog's from
`account.manageKeysUrl` on `GET …/credential`, the setup wizard's from
`inference.keys_url` on `GET /api/v1/setup`. None of those is a production
constant any more: a host on staging sends its operator to mint a key on
staging, presents that key to staging, and offers the one-click grant (which
needs a company to scope to, so the wizard cannot) ahead of the paste field
wherever `hubLink` is true.

Two things the grant deliberately cannot do are **revoke** the key it minted and
**pay** for what that key spends. Both end an errand somewhere this console has
no business being — one withdraws an instance's access, the other moves money —
so both are links out to the hub's own dashboard, behind that person's own
sign-in:

| Page | Path |
|---|---|
| Choose a provider and approve a grant | `{site}/connect?…` |
| Manage API keys — see, name, revoke | `{site}/dashboard?tab=api-keys` |
| Top up the balance those keys spend | `{site}/dashboard?tab=billing` |

`GET …/credential` carries them as `account.manageKeysUrl` and
`account.topUpUrl`, resolved on the **host**. The console never assembles them,
because only the host knows which hub it was pointed at: a link built in the
browser would send an operator working on staging to production's billing page,
where the top-up would arrive in the wrong account and look like it had simply
not arrived.

`{site}` is derived from `api_url` by the ecosystem's naming convention
(`server::hub_account`): `api.tinyhumans.ai` → `tinyhumans.ai`,
`staging-api.tinyhumans.ai` → `staging.tinyhumans.ai`. A backend the convention
does not describe — self-hosted, loopback — derives nothing, `account` is absent,
and the console renders no link rather than one pointing at a host that need not
exist. `TINYHUMANS_WEB_URL` states the site outright where that is wrong.

## Where a connection lives

On the backend, keyed by the account the bearer resolves to — under this model,
the company's owning account. Nothing about a connection is scoped to the member
who made it, and nothing new is stored on the instance.

That is what makes "connect Gmail once, every teammate's agents can use it" true:
every agent in the company resolves the *same* credential from the *company's*
store, so the backend resolves one entity for all of them. It is a property of
the resolution, not a feature layered on top of it.

## Resolution — one seam

`company::company_key::resolve` is the only place a company identity is
resolved. Most specific first:

1. **The company's own key** — `tinyhumans/key` in its `SecretStore`, set by an
   admin through the console.
2. **This instance's platform identity** — `TinyhumansTokenSource`: a projected,
   audience-bound pod token the cluster rotates in place, else a static
   `TINYHUMANS_API_KEY`. Unchanged behaviour for a tenant whose admin set
   nothing.
3. **Nothing** ⇒ fail closed. No tools are wired, and the read planes report the
   degraded state rather than offering a picker that cannot work. An absent
   credential means "no tools", never a borrowed identity.

### An unreadable store is not "no key"

A `SecretStore` read error **propagates** rather than falling through to the next
tier. The tempting shortcut is to map it to "nothing stored" so a transient
hiccup cannot brick a roster build — right about availability, wrong about
attribution.

A connection lives on the backend keyed by the account the bearer resolves to.
If an unreadable store silently resolved a company that *has* a key to the
instance's identity, any connection established in that window would belong to
the instance's account. When the store recovered, resolution would return the
company key, the bearer would resolve to a different entity, and the connection
made under the fallback would no longer be the one the company sees — the same
"connect Gmail" click producing a different owner depending on store health at
that instant, with no signal either way.

Availability-degrade and identity-degrade are different decisions, and only the
first is safe to make silently. So each caller answers the error in the way its
own surface can afford:

- **The roster build** (`TenantComposio::resolve`) logs a warning and withholds
  the tools for that cycle. Fail closed: an *unknown* credential must no more
  mean a borrowed identity than an absent one does. The company loses Composio
  for a cycle and gets it back on the next.
- **The console planes** (`GET …/composio`, `GET …/credential`) surface the
  failure instead of reporting a confident "not configured" for a company that
  may well have a key.
- **`POST …/composio/authorize`** — the call that actually *establishes* a
  connection — refuses outright. It resolves the credential itself rather than
  through the roster path, precisely so a store error stays distinguishable from
  "nothing configured" and cannot be guessed past.

A surface may prepend its **own** escape hatch above that seam. Composio keeps
its BYO `composio/tinyhumans/key` for a company that insists on using its own
Composio account, so its full order is `composio/tinyhumans/key` → company key
→ instance identity → none. What no surface may do is resolve a *company* identity some
other way.

That composed order is itself derived **once**, in
`company::composio::resolve_credential` — not in the harness. The agent-facing
`TenantComposio::resolve` builds its config from it, and the console's
`GET …/composio` reports its `source`, so the tier an operator is shown cannot
disagree with the identity the agents actually present. It lives in the
always-compiled `company::` module rather than the feature-gated `harness::`
one precisely so both callers can share it in every build; a console route that
restated the precedence instead would keep confidently reporting a tier after
the resolver stopped honouring it.

## BYOK Composio: a route, not a tier

Everything above answers *whose identity* a brokered call presents. Composio
carries a second, orthogonal question — *which host* it is presented to — and
that one is not a tier at all.

| | managed | byok |
| --- | --- | --- |
| Host | the OpenHuman backend's `/agent-integrations/composio/*` | `backend.composio.dev` |
| Credential | the precedence chain above | this company's own Composio API key (`composio/byok/key`) |
| Who bills | the platform | whoever owns that Composio account |
| Toolkit gate | the backend's server-enforced allowlist | the company's own Composio dashboard |

The mode is stored under `composio/mode` and defaults to `managed`, so a
company that configures nothing is unaffected by any of this. Storing an API
key through `PUT …/composio/api-key` selects `byok`; clearing it returns the
company to `managed`. The two writes are one call deliberately: a mode without
a key is a company with no Composio tools, and a key without a mode is a
credential nothing reads.

`resolve_access` is the one derivation of both answers, and every reader —
`TenantComposio::resolve`, `GET …/composio`, `/capabilities` — asks it rather
than restating the rule, for the same reason `resolve_credential` is derived
once.

**BYOK does not fall back.** With the mode set and no key stored, the answer is
`Credential::None` and the company gets no Composio tools — it does not quietly
drop to the managed chain. Falling back would connect providers into the wrong
Composio tenant and bill the wrong party, which is a worse failure than having
no tools. The mode is folded into the roster fingerprint, so a switch in either
direction reaches the agents on their next turn with no restart.

One thing the managed route carries and BYOK does not: per-toolkit
`extra_params` at authorize time. The v3 link call takes no such field, and a
BYOK operator sets those on the auth config in their own Composio account.

**Revoking is not on that list**, though it was until it was checked. The
vendored `ComposioTool` has no delete method and OpenHuman's direct mode routes
its revoke through the backend — neither of which says anything about Composio,
which exposes `DELETE /api/v3/connected_accounts/{id}` perfectly well (a no-auth
probe answers `401` on it and `404` on a route that does not exist). Taking a
gap in borrowed code for a gap in the product is how a console comes to hide a
control that would have worked.

### The catalog is OpenHuman's, even under BYOK

A BYOK company is offered the **same 123 providers a managed one is**, fetched
from OpenHuman's backend with the *managed-chain* credential
(`TenantComposio::catalog`) while every Composio call still goes direct.
Switching a company to its own Composio account changes **who it acts as**, not
what the console lets it browse.

Neither list a BYOK company can reach alone is the right one: Composio's own
directory is 1501 entries, most of which this harness has no curated tool
surface for, and the compiled-in shortlist (`agent_ready_toolkits`) is 31.
OpenHuman's backend publishes the middle answer.

The two credentials are kept strictly apart — the Composio key is what calls
present, the managed bearer is *only* ever the curated list's — and both join
the scrub vector, because both are live credentials on that call. The list
itself is non-secret and cached for `CATALOG_TTL`; no Composio traffic is
proxied through it and nothing is billed by it.

With **no managed tier at all** (a standalone host carrying no TinyHumans
identity) there is no curated list to fetch, and the catalog falls back to the
company's own Composio directory rather than presenting the Composio key to the
OpenHuman backend. That fallback is where the bound below applies.

### That fallback is bounded, and says so

Composio's v3 listings are cursor-paginated and large: **1501 toolkits over 8
pages**, and an unscoped `/tools` query is 52,268 over 262. Following the
toolkit cursor to the end measures ~8.4s, against a 5s budget on the host
(`composio_toolkits::FETCH_TIMEOUT`) and another 5s on the console
(`COMPOSIO_PROBE_TIMEOUT_MS`) — so a complete sweep would not merely be slow,
it would always time out and degrade to the built-in fallback list.

`MAX_PAGES` is therefore 3 (~600 toolkits, ~3.3s measured), and what it could
not reach is **counted from Composio's own `total_items` and logged**, never
dropped in silence: a grid showing 600 providers is indistinguishable from a
complete one. Serving the whole directory would mean refreshing it off the
request path rather than raising either budget; that is a follow-up, not
something a larger timeout fixes.

One parameter is worth knowing about, because Composio v3 **ignores unknown
query parameters rather than refusing them**: tools are scoped with
`toolkit_slug=a,b`, not `toolkits=`. The wrong spelling does not error — it
returns the first page of all 52,268 tools alphabetically. Repeating the
parameter returns an empty body; one comma-separated value is the working form.

This mirrors OpenHuman's own `backend` / `direct` split
(`vendor/openhuman/src/openhuman/integrations/composio/client.rs`); the
vocabulary here is `managed` / `byok` to match `company::search`, which made the
same choice first.

## Rotation

The rotation guarantee — "rotating the company key does not silently leave one
brokered surface on the old credential" — is structural rather than a
convention. Because every brokered surface calls the one resolver, there is no
second resolution that could drift, and no surface that could forget to re-read.

Two mechanics make it land without a restart:

- The key is read **live** from the secret store on every resolution, so a set /
  rotate / clear takes effect on the next cycle.
- The resolved credential contributes its **value** to the harness roster
  fingerprint (`Credential::hash_identity`), so a rotation rebuilds the tool
  roster. This is deliberately different from the projected platform token,
  which contributes its *path*: the cluster rotates that one every few minutes
  and hashing its value would rebuild every agent's roster on that schedule. A
  company key is rotated by a person, on purpose, and a new value really is a
  new identity.

**The fingerprint is internal and must stay internal.** It is a value-derived
hash of a live credential, which makes it a cheap confirmation oracle for anyone
who can read it: given a guess at the key, you can confirm it. It lives only in
`HarnessPool`'s in-memory `RwLock<HashMap<CompanyId, u64>>` and is compared for
equality — no `tracing` call renders it, no DTO serializes it, and no journal
event carries it. Do not log it, return it from a route, or put it in an event,
even for debugging; if a rebuild needs explaining, log *that the identity
changed*, never the hash.

## Write-only, and admin-only

The key is sent on the `key` field, stored, and never echoed. No read route
returns it; `GET …/credential` carries `configured` plus `source` — one of
`company` / `attested` / `static` / `none`, the same vocabulary `GET …/composio`
and the connections read plane already use. `Credential`'s `Debug` redacts it,
and the Composio tools feed whatever value they resolve to the scrubber as a
known secret, so it cannot survive into agent-visible output.

`PUT` requires an admin. This key is the identity every one of the company's
agents presents **and** the account they all spend against, so setting it is a
decision made for the company rather than a member's own — the same reasoning
that made `PUT …/composio/token` admin-only in issue #403. Both a set and a
clear are journaled as `ToolAccessChanged`, told apart from each other, and
attributed to whoever made the change.

## Which connected account (issue #820)

The credential decides **whose** accounts a call can reach. It does not decide
**which** of them, and for a company holding two accounts for one toolkit —
`ops@` and `billing@` Gmail — those are different questions.

Until #820 the second had no answer at all. `composio_execute` built its body as
`{tool, arguments}` and carried no connection id, so the account was resolved by
Composio for the entity, outside this codebase entirely. Two consequences worth
naming: "send from the billing account, not ops" was not sayable, and *which
Gmail did the agent send from* was unanswerable even after the fact. The only
lever was to disconnect the account you did not want.

The choice is now a per-company, per-toolkit preference:

- **Stored** as one JSON blob under `composio/defaults`
  (`{"gmail": "ca_billing"}`), beside the credential it qualifies and read the
  same way `inference/config` is. Not a secret — the ids are the same ones
  `GET …/composio/connections` already hands the console, and are useless
  without the bearer that scopes them — but company state, so it moves, backs up
  and is deleted with the rest of the company's Composio state.
- **Resolved** into `TenantComposio` by the same `resolve` the credential goes
  through, and folded into the roster fingerprint, so a change reaches the
  agents on their next turn with no restart — exactly like a rotated token.
- **Sent** as `connectionId` on the execute body, which the platform backend
  forwards to Composio as `connectedAccountId`.
- **Set** through `PUT …/composio/connections/{id}/default` (admin-only), which
  validates the id against this company's own filtered connection list first and
  refuses an account that is not usable. Cleared through the matching `DELETE`,
  which deliberately makes **no** upstream call: clearing has to work when the
  account is gone or the provider is unreachable, which is when a validating
  clear would refuse.

**Absent is the ordinary state, and it is not a degraded one.** A company that
has chosen nothing sends no connection id and gets Composio's own resolution,
byte-for-byte the behaviour that existed before — which is what keeps this
change invisible to every single-account company. Nothing invents a default from
the connection list: `list_connections_detailed`'s `(toolkit, id)` sort is a
stable render order for a read, never a choice, and a default the console
claimed but the harness did not honour would read as a guarantee. The console
says "Composio picks" rather than pointing at a row.

Two pins are dropped automatically, because a pin to a connection that no longer
exists would be sent on the next execute and refused — turning the disconnect of
one account into a broken toolkit: when the console revokes an account, and when
`GET …/composio/connections` finds a chosen id that Composio no longer lists.

## Not the inference key

`inference/key` is a different thing and must stay a different slot. It holds
whatever credential the company's *declared provider* wants — an OpenRouter
`sk-or-…`, a raw BYOK token, an `openai_compatible` key. It is provider-scoped,
not an identity, and handing it to the TinyHumans backend would present one
vendor's credential to another.

Since `managed`'s removal the two never coincide, which makes the separation
cleaner rather than looser. A company holding **no** inference key rides the
subscription on the platform's own credential — resolved from this host's
identity, not from `inference/key` — and a company that sets one is naming an
OpenRouter account that has nothing to do with TinyHumans. See
[providers.md](providers.md).

There is one such slot **per harness**: the default harness keeps the flat
`inference/key`, and every named one uses `harness/<id>/inference/key`. The
asymmetry is deliberate — the `SecretStore` has no rename, so namespacing the
default too would orphan the stored credential of every company already
running.

## What this does not cover

- **Legacy native OAuth credentials.** #838 retires
  `…/connections/{provider}/start`: through 2026-09-30 it answers a dated
  `410 native_oauth_retired`, then #1023 removes it. The callback likewise
  explains an in-flight browser redirect without exchanging its code. Existing
  `oauth/{provider}` values remain readable and revocable, but no agent can use
  them and no credential tier treats a configured host provider app as a route.
- **Chat inference and embeddings.** Both still resolve from the environment via
  `hosted_endpoint_from_env`. Moving them onto this seam is issue #585; when it
  lands they inherit the rotation guarantee by construction, because the seam is
  already here.
- **Media generation.** Deliberately environment-only: it runs on the
  *platform's* managed credential, never a company-controlled one. Managed
  `web_search` now reads the company's copied TinyHumans key first and the
  environment credential second. A separate BYO search-provider key still
  replaces the managed tool entirely; see [search.md](search.md).

## Known limits, recorded deliberately

- **No per-member attribution.** Spend arrives as one account, so which member
  burned what is not answerable. Per-agent `budget_usd_daily` caps still work;
  per-person accounting does not exist and is not in scope.
- **Removing someone from the roster stops future access**, but nothing already
  spent is separable.
- **Two companies pasting the same key share one entity.** That cannot be
  prevented client-side; it is a deployment caveat, the same one the BYO Composio
  token already carries.
- **Media generation does not read the projected tier.** `web_search`, chat
  inference and embeddings all resolve through `TinyhumansTokenSource`, so a
  hosted tenant's rotating pod token reaches them. `media_backend_from_env` does
  not: it reads `OPENCOMPANY_MEDIA_KEY`, else a static `TINYHUMANS_API_KEY`, and
  a projected token file alone leaves media unwired. This is a migration miss
  from #189 rather than a decision, but it cannot be closed here — the upstream
  media client takes a `String` bearer for the life of the process, so flattening
  a 600-second projected token into it would trade "never works" for "works for
  ten minutes". Fixing it properly means giving that client a resolvable
  credential upstream, the same shape `SearchBackend` already has. Until then a
  hosted deployment that wants media must also carry a **non-projected** key, and
  `PlatformCredentialStatus::boot_warning` says so at boot (#879). That key
  should be `OPENCOMPANY_MEDIA_KEY`, the supported per-surface override. A static
  `TINYHUMANS_API_KEY` would also work, but it is the `docker compose` credential
  and the explicitly unsupported self-host hatch; reaching for it here would make
  that hatch load-bearing on the hosted path, which is a decision for whoever
  closes the upstream gap rather than a workaround to settle by default.
