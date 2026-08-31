# Product analytics

**Status: implemented (issue #1739).** What OpenCompany reports about how it is
being used, what it deliberately never reports, which installs report at all,
and how to turn it off.

The short version, and the only three sentences most readers need:

- A **desktop or self-hosted install sends nothing.** Not "sends nothing by
  default" in the sense of a flag someone could flip in a config file — the
  network client is behind a cargo feature the shipped default build does not
  compile, so there is no code in that binary that could make the request.
  Getting one out of that state takes a **recompile**: `--features analytics`
  *and* an explicit `OPENCOMPANY_ANALYTICS=on`, both deliberate, and neither
  reachable from anything a shipped binary reads at runtime. See
  [Configuration](#configuration) for the four conditions in full.
- A **hosted tenant** — a container the OpenCompany platform provisioned and
  operates — reports **shape and outcome only**, under an **opaque id**.
- Nothing an operator or an agent wrote ever leaves the process this way. Not
  message text, prompts, file names, ledger values, tool arguments, addresses,
  or company names.

## Why the default is silence

This repository is GPL-3.0 and self-hostable. An open-source instance that
phones home by default is a betrayal of that, whatever the payload contains.

It is also the posture the rest of the tree already takes. `tests/offline_e2e.rs`
runs inside a network namespace with no routes and asserts that the jail holds;
[offline.md](offline.md) says outright that a new cloud call on a shared path
*should* turn that lane red, and that widening the namespace to make it pass is
not an option. An analytics client firing at boot would do exactly that. The
feature gate is what keeps both things true at once.

And [`roadmap.md`](../roadmap.md)'s non-goal — "no private feedback backend" —
still holds and is unchanged by this: **feedback** goes to public GitHub issues
or stays local, and never rides this channel.

## What is collected

Three events today. Each carries the context envelope below.

| Event | Fired when | Properties |
|---|---|---|
| `instance_started` | the host finished booting and registered its companies | `companies` (count), `storage` (`fs`/`sqlite`/`mongodb`), `setup_complete` |
| `turn_finished` | one cycle — the product's unit of work — ended | `trigger`, `outcome` (`ok`/`failed`), `failure` (coarse class), `duration_ms`, `effects_executed`, `approvals_parked` |
| `turn_metered` | one usage sample was recorded | `sample_kind`, `provider`, `model` (omitted when the sample named none), `input_tokens`, `output_tokens`, `cached_input_tokens`, `cost_usd`, `attributed_to_run` |

`model` is the closed `ModelSlug` vocabulary from
[`metering`](../../../src/metering/model.rs) (issue #1749), already folded at the
harness — `anthropic-sonnet`, `openai-gpt`, `chat-v1`, …, or `other` for a model
this build cannot name. It is forwarded, never re-classified: `ModelSlug`'s inner
value is a compiled-in literal, so a BYOK tenant's raw model name cannot reach a
payload even in principle, and analytics needs no classifier of its own for it.

The property is **omitted** when the sample named no model — every OAuth and
search call, a cognition path that cannot identify one, and any row written
before the field existed. Absent rather than `other`, so "no model ran" stays a
different answer from "a model ran that this build cannot name"; collapsing them
would inflate the `other` bucket with every tool call.

### The context envelope

Set once at boot, attached to every event:

| Property | Value |
|---|---|
| `distinct_id` | the opaque identity — see below |
| `deployment` | `desktop` \| `self-hosted` \| `hosted-tenant` |
| `app_version` | the crate version |
| `os`, `arch` | `std::env::consts` |
| `cognition_path` | `harness` \| `hosted` \| `echo` \| `sidecar` \| `custom` |
| `cognition_provider` | `openrouter` \| `subscription` \| `managed` \| `ollama` \| `byok` \| … |
| `cognition_metering` | `per-turn` \| `per-cycle` \| `none` |
| `harness_in_build`, `mcp_in_build`, `acp_in_build`, `oauth_in_build`, `analytics_in_build` | the compiled feature set |

`cognition_*` is read off [`ports::brain::Cognition`](ports-cognition.md), the
descriptor the runtime already keeps, rather than re-derived from configuration
beside the code that picks a brain.

## What is never collected

Message text. Prompts. Agent output. File and workspace paths. Ledger values and
row contents. Tool names and tool arguments. Email addresses. MCP server names.
Company names and ids. Agent names. Task titles. Error messages. Credentials of
any kind.

**This is a structural guarantee, not a review-time rule.** A property value in
this crate is one of exactly four things:

```rust
PropValue::Word(&'static str) | Count(u64) | Amount(f64) | Flag(bool)
```

There is no `String` variant and no `serde_json::Value`. A `&'static str` cannot
be produced from runtime data without deliberately leaking memory, so every
textual property is a literal written in this repository. A call site with a
runtime string — a provider slug, an error, a trigger — must pass it through a
classifier that maps it onto a fixed list, and **anything unrecognised becomes
`other`**. The dangerous direction is a value nobody anticipated, so that is the
direction the design fails in.

Two consequences worth stating plainly, because both look like a bug until you
know they are the point:

- An MCP tool call reports the provider `mcp`, not the name the operator gave
  their server. That name is frequently a customer or a project.
- A failure reports a coarse class (`store`, `refused`, `cognition`, …), not the
  error message. `Display` on this crate's error type embeds absolute host
  paths, company ids, tool names, ledger slugs and agent text — it is the
  richest source of user content in the tree.

`src/analytics/test.rs` asserts both halves: that hostile inputs do not survive
into a payload, and that **every** string in a rendered payload is either the
opaque id, a platform constant, or a word from a hand-written vocabulary.

## Identity

`distinct_id` is opaque and stable, and it is one of two things:

- `t_<32 hex>` — an **HMAC-SHA256 of the tenant slug**, for a hosted tenant, and
  only when the platform also supplies `OPENCOMPANY_ANALYTICS_ID_KEY`. Hashed
  rather than passed through because a tenant slug is usually the customer's own
  brand. Every question analytics asks — uniques, funnels, segmentation,
  retention — needs only that the same tenant maps to the same value every time.

  **Keyed, not a bare digest.** The slug space is small, guessable and often
  public, so a plain SHA-256 is a lookup table away from being reversible: an
  attacker who can list plausible customer names recovers the mapping offline.
  The key removes that, at the cost of making it load-bearing — rotating it
  re-identifies every tenant and breaks retention continuity, so it is minted
  once per platform and kept.

  Both halves are required. `boot::identify` emits `t_` only when a tenant
  namespace **and** the key are present; with either missing it falls back to the
  `i_` form below rather than emitting a weaker identity. A platform operator who
  omits the key therefore gets per-instance identity and per-instance retention,
  which is a quieter outcome than they may expect — the boot line does not call
  it out, and this is the place that says so.
- `i_<32 hex>` — this host's **instance id** otherwise: 16 random bytes minted
  on first boot and persisted under the data root
  ([data-root.md](data-root.md)). Random, not derived: `src/app/instance.rs`
  argues that at length, and the reasons are the same here.

One caveat, inherited from that module: on an **unwritable data root** the
instance id cannot be persisted and a fresh one is minted per process, so such a
host looks like a new install on every boot. It logs a warning when this
happens. A read-only root is a misconfiguration ([storage.md](storage.md)); the
symptom in analytics is inflated install counts, not lost data.

## Configuration

| Variable | Meaning |
|---|---|
| `OPENCOMPANY_DEPLOYMENT` | `desktop` \| `self-hosted` \| `hosted-tenant`. Declared by whoever launches the process. Default and fallback: `self-hosted`, including when the declared value cannot be read. |
| `OPENCOMPANY_ANALYTICS` | `on` forces reporting; `off` forbids it and outranks everything else. |
| `OPENCOMPANY_ANALYTICS_TOKEN` | the Mixpanel project token. **Configuration, never a compiled-in constant** — a token baked into a public binary is a token everyone has. |
| `OPENCOMPANY_ANALYTICS_ENDPOINT` | overrides the collector URL. Must be an absolute `http`/`https` URL with a host; anything else is silence with a reason. |
| `OPENCOMPANY_ANALYTICS_ID_KEY` | the secret a hosted tenant's analytics id is derived under. Injected by the platform, never given to the collector. Absent means the host is known by its random instance id instead. |

Reporting happens only when **all** of these hold:

1. the binary was built with `--features analytics`;
2. `OPENCOMPANY_ANALYTICS` is not `off`;
3. the deployment is `hosted-tenant`, **or** `OPENCOMPANY_ANALYTICS=on`;
4. a project token is configured;
5. the collector endpoint is one a client could actually POST to.

The endpoint is validated with `url`, the same parser `reqwest` uses, rather
than an approximation of the URL grammar. The first attempt hand-rolled the
check and accepted eight shapes `reqwest` rejects — `http://[::1/track`,
`:99999`, `:65536`, `:abc`, `host:8080:9090`, `]::1[`, `127.0.0.1.5` and
`999.999.999.999` — each of which resolved to reporting and then dropped every
batch, which is the failure the check exists to prevent. Issue #673 had already
settled this rule for a different call site: it must be *the same* parser
`reqwest` uses, because a second hand-rolled reader is a bypass waiting to be
found.

Condition 1 is met in exactly one place in this repository: `TENANT_FEATURES` in
`.github/workflows/deploy-staging.yml`, the hosted tenant image's feature set.
Nothing else compiles the feature — not the desktop (`src-tauri/Cargo.toml`),
not the default build, not any CI lane but the scoped analytics one. A hosted
image whose feature list drops `analytics` reports nothing however the manager
configures it, and says so at boot rather than failing quietly.

`OPENCOMPANY_TENANT_ID` implies `hosted-tenant` when `OPENCOMPANY_DEPLOYMENT`
says **nothing at all** — the control plane injects it and nothing else does.
That is the only inference taken. A declaration that is present but unusable —
an unknown slug, or bytes this process cannot decode — is not "nothing": it wins
over the inference and resolves to `self-hosted`. Reading it through
`EnvSource::get` rather than `get_os` made a non-UTF-8 value indistinguishable
from an absent one, so an explicitly-declared shared-single-DB tenant fell
through to the inference and came back `hosted-tenant` — reporting switched
**on** by a malformed variable, on the discriminator every other decision here
rests on. A **blank** declaration is still absent, so a launcher that exports an
empty variable changes nothing. A discriminator sniffed from something incidental (the
data dir, the bind address, `harness_in_build`) inverts the day someone changes
an unrelated setting, silently, and points at the wrong file.

An unrecognised value for either switch resolves to **silence**, never to
reporting — on a hosted tenant too. Both directions of that typo matter and only
one is obvious. A typo must not *upgrade* an install into one that reports; it
must also not fail to *downgrade* one, which is what happened while an
unreadable value fell through to the deployment default: an operator who meant
`OPENCOMPANY_ANALYTICS=off` and typed `of` kept reporting, and their boot line
said "reporting to …" rather than anything that would send them back to look.
Silence is the answer to "I cannot tell what you asked for", and the boot line
names the reason. A **blank** value is treated as absent rather than unreadable,
so a launcher that exports an empty variable changes nothing.

### How to turn it off

Set `OPENCOMPANY_ANALYTICS=off`. It outranks the deployment kind and the token,
and it is the first thing checked. Boot prints one line either way:

```text
analytics: off (not a hosted tenant and no explicit opt-in)
analytics: off (operator opted out)
analytics: off (the OPENCOMPANY_ANALYTICS value is not recognised)
analytics: off (the OPENCOMPANY_ANALYTICS_ENDPOINT value is not a usable http(s) URL)
analytics: off (reporting to https://api.mixpanel.com/track was configured, but this build was compiled without the `analytics` feature)
analytics: reporting to https://api.mixpanel.com/track
```

The fourth of those is the endpoint check. `OPENCOMPANY_ANALYTICS_ENDPOINT` is
validated where the decision is made, not where the send is attempted:
`collector.internal/track` — a proxy hostname written without a scheme, which is
how anyone writes one the first time — used to resolve to reporting, so boot
announced "reporting to collector.internal/track" and every batch then died
inside `reqwest` behind a `debug!` line no operator has enabled. The product said
something true-sounding and did nothing. Bytes that are not valid UTF-8 are
rejected the same way rather than falling back to the default endpoint: a tenant
that pointed analytics at its own proxy and mistyped it would otherwise have
reported to Mixpanel instead, which is telemetry sent somewhere nobody
configured. The reason line never quotes the rejected value, for the reason
below.

The endpoint is named; the token never is — and the endpoint is named
**sanitized**. `OPENCOMPANY_ANALYTICS_ENDPOINT` exists so a deployment can front
Mixpanel with its own proxy, and an authenticated proxy carries its key in the
two places a URL can hold one: userinfo (`https://user:pass@host/track`) and the
query string (`?key=…`). Both are stripped before the line is printed, leaving
scheme, host and path, and the line says `(credentials redacted)` when it
shortened anything — a silently truncated URL is its own hour of confusion. The
`ProjectToken` redaction does not cover this; it guards a different string.

The same URL reaches one other log line: the `debug!` the transport writes when
a send fails. `reqwest::Error` retains the request URL and prints it, so an
unreachable collector wrote the proxy key into container logs by a path the boot
line's redaction never touched. Measured against reqwest 0.12.28, userinfo is
already stripped from what it prints and **the query string is not** — so `?key=…`
was leaking and `user:pass@` was not. The transport calls `without_url`, which
removes the URL rather than rewriting it, so neither shape can reach the line
whatever a future reqwest decides to print; the destination on that same line
comes from the one `loggable_endpoint` helper the boot line uses, so there is no
second redaction to fall out of step with the first.

The fourth line is the one worth reading twice. It reports what the process will
**do**, not what was configured: a build without the `analytics` feature
resolves to reporting and then gets a `NullTracker`, because there is no
transport in it to hand back. Saying "reporting to …" there would be the exact
opposite of the truth, and the `mixpanel::build` line that explains it is a
`tracing::info!` the CLI's default `EnvFilter` swallows — which is why every
boot line here is a `println!` in the first place.

### Tenant identity is keyed, not merely hashed

A hosted tenant's `distinct_id` is an HMAC-SHA256 of its slug under
`OPENCOMPANY_ANALYTICS_ID_KEY`, truncated to 128 bits and prefixed `t_`.

It used to be a plain `SHA-256(slug)`, and that did not deliver what it
promised. A hash only hides an input that cannot be guessed, and a tenant slug
is close to the opposite: it is usually the customer's brand, drawn from a
small, public, enumerable set. Anyone holding the digests — the collector
itself, or anyone with access to the analytics project — can hash a few thousand
candidate brands and read `t_<digest>` straight back to the customer. Truncation
does not help. Nor would a salt compiled into the binary, since this is a
GPL-3.0 crate and that salt would ship in every copy of the source.

**There is no unkeyed fallback.** When no key is configured the host is known by
its own random instance id (`i_…`, 128 random bits from `app::instance`), which
identifies nobody's customer. That is the safe direction: a host that cannot
identify its tenant privately identifies *itself* rather than identifying its
customer publicly. Every question the identity exists to serve — uniques,
funnels, segmentation, retention — is answered by either id, because the
instance id is persisted in the data root and is therefore stable across
restarts.

The consequence for the platform: **until the manager injects
`OPENCOMPANY_ANALYTICS_ID_KEY`, hosted tenants report under instance ids rather
than tenant digests.** Grouping several instances of one tenant together needs
the key; the manager can still correlate a digest back to a tenant itself,
because it holds both the key and the slug. The collector cannot.

## Where it hooks in

| Seam | File | Why there |
|---|---|---|
| `turn_finished` | `runtime::cycle::CycleRunner::run_bracketed` | The cycle's whole span, including the wait on the per-company serial lock — which is the part an operator experiences as "nothing is happening". |
| `turn_metered` | `analytics::meter::TrackingUsageMeter`, a decorator over the `UsageMeter` port | Every `metering::record_*` path ends there, on every build. The harness cost hook is richer but `openhuman`-gated, and the cycle-level path deliberately reports zero tokens on that build so spend is not double-counted — so an event at either one is blind on the other half of the fleet. |
| `instance_started` | `analytics::boot::install` | After companies register **and after the port is bound**: the company count and the cognition path are not known before the first, and a host that never took its address never started in any sense worth counting. |
| cognition relabel | `Tracker::observe_cognition`, from `server::provision` and `runtime::rebuild` | Boot's answer stops being true in two ways. A hosted host provisioned into an empty registry had no runtime to read and recorded `custom`/`unknown`; and a company that configures inference for the first time is rebuilt in place (issue #290), which moves it from `echo` to `harness`. Most recent observation wins. Events already sent are not revised. |
| `flush` | `src/bin/opencompany.rs`, after the bound host stops serving | The server has already drained, so a last-moment turn's event still leaves — bounded by `shutdown::FLUSH_BUDGET`, below. |

**Cognition is a host-level label, and on a multi-company host it is
approximate.** Inference is configured per company, so a host serving two
companies — one configured, one on the echo fallback — has two cognition paths
and one envelope, and whichever was observed last answers for both. Making it
exact means moving cognition off the envelope's super-properties and onto
`turn_finished` and `turn_metered` themselves, which changes the payload shape
this document describes and does not fit `instance_started`, which has no
company. That is an analytics-contract decision rather than a defect, raised in
review on PR #1751 and left for its own change.

**The flush is bounded, and telemetry is what gives way.** `DEFAULT_GRACE`
(25s) and `CONNECTION_GRACE` (2s) are sized to land at 27s, deliberately under
Kubernetes' default 30s `terminationGracePeriodSeconds`. The flush talks to a
collector this process does not control, with a 5s client timeout of its own, so
unbounded it took the worst case to 32s — buying a `SIGKILL` in the middle of the
drain those 27s exist to protect.

`shutdown::flush_budget(drain)` gives it whatever is left of the pod's default
30s after the drain and the connection window, capped at
`shutdown::FLUSH_BUDGET` (2s). Derived rather than added, because the drain is
configurable: a flat two seconds on top of `OPENCOMPANY_SHUTDOWN_GRACE_SECONDS=28`
— which fits in 30s exactly on its own — took it to 32 and recreated the same
`SIGKILL`, for a value the operator had every reason to think was safe. A drain
that already fills the budget leaves zero and the flush is skipped, and a flush
that does not finish is abandoned. A dropped batch costs a line in a dashboard;
an overrun costs a half-finished turn.

Failure is silent by construction: `Tracker::track` is synchronous and
infallible and returns nothing, so a call site cannot await a network or branch
on a telemetry error. A dead collector drops batches after one `debug!` line.
The buffer is bounded at 500 events — if the collector is unreachable long
enough to fill it, the right outcome is losing telemetry, not a tenant
container.

## What is deliberately not instrumented yet

Named so the gaps are countable rather than implied. Each is a follow-up, not an
oversight:

- approvals, tools, workflows, ledgers, connections/MCP, and console views —
  the surfaces #1739 lists as candidates;
- **build commit**, which this crate does not stamp at build time;
- a timed flush shared with `MaintenanceTicker` rather than the transport's own
  30-second loop.

## Testing

| Lane | Command |
|---|---|
| default build — the decision, the vocabulary, the payload builder, the meter decorator, the cycle hook | `cargo test --locked` |
| gated — the transport, and the acceptance criteria that need it | `scripts/ci/run-scoped-suite.sh "analytics" analytics analytics` |

`scripts/ci/feature-lanes.txt` records the second as `partial`, per the rule in
[`CLAUDE.md`](../../../CLAUDE.md) that every feature says which lane runs its
tests.

The two tests that matter most are a pair, and they only mean something
together: `a_self_hosted_build_makes_no_request` stands up a local collector,
hands the process a token and an endpoint, declares no deployment, and asserts
**zero** requests; `a_hosted_tenant_reports_with_the_full_envelope` is its
positive control against the same collector, the same events and the same code
path with one variable changed. Without the second, a zero request count would
be indistinguishable from a test that never sends anything at all.
