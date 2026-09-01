# Product analytics

**Status: implemented (issue #1739).** What OpenCompany reports about how it is
being used, what it deliberately never reports, and where each event is raised.
Which installs report at all, and how to turn it off, is in
[analytics-configuration.md](analytics-configuration.md).

The short version, and the only four points most readers need:

- A **desktop install sends nothing, and cannot.** Not "sends nothing by
  default" in the sense of a flag someone could flip in a config file — the
  network client is behind a cargo feature `src-tauri/Cargo.toml` does not
  compile, so there is no code in that binary that could make the request.
  Getting one out of that state takes a **recompile**: `--features analytics`
  *and* an explicit `OPENCOMPANY_ANALYTICS=on`, both deliberate, and neither
  reachable from anything a shipped binary reads at runtime.
- A **self-hosted install sends nothing**, and since 2026-08-29 how strong that
  sentence is depends on how the install was built. Built from source with the
  default feature set it is the desktop's *cannot*. Built from this
  repository's `Dockerfile` — whose `ARG FEATURES` now defaults to `analytics`,
  because that image is the hosted tenant workload — the transport is compiled
  and the guarantee is a **will not**: `analytics::config::resolve` still
  requires a hosted tenant, or an explicit `OPENCOMPANY_ANALYTICS=on`, **and** a
  token, and `OPENCOMPANY_ANALYTICS=off` still outranks both. An operator who
  runs that image with no tenant namespace and no token sends exactly what they
  sent before: nothing.
  [analytics-configuration.md](analytics-configuration.md) has the five
  conditions in full, and says why the weaker promise is the right trade for
  that one artifact.
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

Ten events today. Each carries the context envelope below.

| Event | Fired when | Properties |
|---|---|---|
| `instance_started` | the host finished booting and registered its companies | `companies` (count), `storage` (`fs`/`sqlite`/`mongodb`), `setup_complete` |
| `turn_finished` | one cycle — the product's unit of work — ended | `trigger`, `outcome` (`ok`/`failed`), `failure` (coarse class), `duration_ms`, `effects_executed`, `approvals_parked` |
| `turn_metered` | one usage sample was recorded | `sample_kind`, `provider`, `model` (omitted when the sample named none), `input_tokens`, `output_tokens`, `cached_input_tokens`, `cost_usd`, `attributed_to_run` |
| `approval_parked` | an effect was parked for an operator decision | `group` (`spend`/`send`/`sign`/`publish`/`hire`/`identity`/`other`), `priced` |
| `approval_decided` | a parked approval was settled | `group`, `verdict` (`approved`/`denied`/`expired`), `waited_ms` |
| `tool_called` | one tool call finished | `source` (`built-in`/`mcp`/`composio`/`other`), `outcome`, `duration_ms` |
| `workflow_run_finished` | one workflow run reached a terminal state | `status` (`completed`/`failed`/`blocked`/`cancelled`/`other`), `nodes`, `duration_ms` |
| `ledger_appended` | a ledger record was appended | `shape` (`tasks`/`goals`/`decisions`/`risks`/`commitments`/`learnings`/`other`), `records` |
| `connection_changed` | a connection or MCP server changed state | `provider`, `transition` (`connected`/`disconnected`/`refreshed`/`failed`) |
| `console_viewed` | the operator opened a console view | `view` |

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

### What the newer words are folds of

The seven events added on 2026-08-29 introduce eight textual properties. Every
one is a classifier output or a compiled-in word, and each is the point where a
name belonging to the company stops:

- **`group`** is the supervised taxonomy (`policy::gate::group_slug`), never
  `effect.kind` and never the payload — the kind is operator- and MCP-supplied
  text and the payload is the effect's arguments. **`verdict`** is one of three
  literals. `priced` is `amount_usd.is_some()` and not the figure: what a
  company is about to spend is a fact about its business, not about the product,
  and `waited_ms` is saturating, because park and decision are separate
  wall-clock reads and an NTP step backwards between them would otherwise wrap a
  short wait into eighteen quintillion milliseconds.
- **`source`** on `tool_called` is a test of the tool name's *shape*, not a
  census of the belt (`harness::built_in::steps::tool_source`). A list of the
  tools this repository builds would be a second spelling of the toolbelt: it
  rots the first time a tool lands behind a cargo feature the reader did not
  have on, and a rotted entry reports real built-in traffic as `other` — which
  reads as "an unknown tool ran". Sub-agent calls are deliberately not reported;
  they are work done *inside* the parent call that spawned them, and counting
  both double-counts exactly the turns that delegate. A call the **approval gate
  parked** is not reported at all: it arrives as `success = false` carrying the
  refusal, which is the same event the operator's timeline reads as
  `AwaitingApproval`, so scoring it `failed` would inflate the failure rate
  hardest for the companies that supervise the most. It is not a success either
  — the tool never ran — and the park is already counted by
  `approval_parked`/`approval_decided`.
- **`status`** on `workflow_run_finished` reads `WorkflowRunVerdict`'s wire
  token rather than matching its variants, so a verdict this vocabulary has not
  heard of degrades to `other`. Five stay there on purpose — `running`,
  `stranded`, `undelivered`, `awaiting-approval`, `degraded` — each *nearly* one
  of the four words, and folding one onto its neighbour would make that
  neighbour's count mean two things with no way to separate them afterwards. A
  run that fails while **warming the roster** — the first run after a boot or a
  rebuild, against a provider that will not construct — reports `failed` with
  zero nodes and zero duration, rather than returning to its caller with no
  event at all; those are the cold-start and misconfiguration failures a failure
  metric exists for, and a company whose workflows had never once warmed would
  otherwise have shown a clean sheet.
- **`shape`** on `ledger_appended` is the six ledgers every company has whichever
  vertical it started from, and never a cell. A ledger slug is author-defined at
  runtime, so `acme-holdings-merger` is a legal one that names the customer; a
  company bundle's own ledgers (`candidates`, `deals`, `matters`, and ninety
  more) fold to `other` beside the runtime-declared ones rather than becoming a
  vocabulary nobody could review as a closed set.
- **`transition`** is one of four compiled-in words, and **`provider`** on
  `connection_changed` is the same `provider_slug` list the envelope uses for
  cognition — one list rather than two that drift. The values in reach of those
  handlers are an OAuth access token, a Composio connection id, an MCP server
  name the operator typed and a provider error message; none of them is offered.
- **`view`** is a console route, and the one event the console raises rather than
  the host — see [Console views](#console-views-come-from-the-console).

### The context envelope

Set once at boot, attached to every event:

| Property | Value |
|---|---|
| `distinct_id` | the opaque identity — see below |
| `deployment` | `desktop` \| `self-hosted` \| `hosted-tenant` |
| `app_version` | the crate version |
| `build_commit` | the object id this binary was stamped from, optionally `-dirty`, or `unknown` |
| `os`, `arch` | `std::env::consts` |
| `cognition_path` | `harness` \| `hosted` \| `echo` \| `sidecar` \| `custom` |
| `cognition_provider` | `openrouter` \| `subscription` \| `managed` \| `ollama` \| `byok` \| … |
| `cognition_metering` | `per-turn` \| `per-cycle` \| `none` |
| `harness_in_build`, `mcp_in_build`, `acp_in_build`, `oauth_in_build`, `analytics_in_build` | the compiled feature set |

`cognition_*` is read off [`ports::brain::Cognition`](ports-cognition.md), the
descriptor the runtime already keeps, rather than re-derived from configuration
beside the code that picks a brain.

`build_commit` is the commit and not just the version, because `app_version` is
the crate version: it moves on a release and not on a deploy, so every build
between two releases is indistinguishable and "did this start after Tuesday's
rollout?" — the question a regression always asks — has no answer. #1771 already
stamps the revision into the binary and `/spec` reports it; it simply never
reached the envelope until 2026-08-29.

It is the one envelope string that is **not** a literal compiled into this
repository, so it is folded like every other. `build_stamp.rs` prefers
`OPENCOMPANY_BUILD_COMMIT` over `git` on purpose — an escape hatch for a build
environment nothing else covers — and it *sanitizes* rather than validates. So
`release-2026-08-25`, or a branch name carrying a customer's name, reaches
`crate::BUILD_COMMIT` intact, and until 2026-08-31 it rode every event this
instance sent.

It no longer does. **`types::commit_slug` folds the value on its way into the
envelope**: it admits only an object id (7–40 hex digits, with at most the
`-dirty` suffix `build_stamp.rs` itself appends) and emits `unknown` for
anything else, so a build stamped `release-2026-08-25` reports `build_commit:
unknown` rather than the label.

The two readings are therefore deliberately different, and both are correct.
`crate::BUILD_COMMIT`, `/spec` and every operator-facing surface still carry the
**raw** stamp, because the person reading them is the person who stamped it and
the label is the whole point of the escape hatch. Analytics carries the folded
one. This narrows what leaves the process as telemetry and takes nothing away
from the builder.

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

### Nor anything the collector adds after the request leaves

The transport appends **`ip=0`** to the URL it posts to. That is Mixpanel's own
switch for request-IP geolocation, and left on, the Track API enriches every
event server-side with `$city`, `$region` and `mp_country_code`.

Those three are the one way to break "shape and outcome, never content" with no
line of code saying so. They never pass through `PropValue`, so the closed
vocabulary above does not constrain them and the payload test that walks every
string in a rendered event cannot see them — the properties are added after the
request leaves. They are not in the payload this document describes, and they
amount to the company's approximate location, which is a fact about the customer
rather than about the product.

A query parameter rather than an `$ip: 0` property on each event, because it
applies to the batch and cannot be forgotten by a call site added later.
Appended per send rather than baked into the configured endpoint, so
`OPENCOMPANY_ANALYTICS_ENDPOINT` stays exactly what the operator wrote — that is
what the boot line prints and what `loggable_endpoint` redacts, and rewriting it
would make the two disagree with the setting. A custom collector that already
carries a query string keeps it, and one that already sets `ip` is left alone:
an operator who wrote that meant it.

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
  it out, and this is the place that says so. What the key is worth, and what
  the platform loses without it, is in
  [analytics-configuration.md](analytics-configuration.md#tenant-identity-is-keyed-not-merely-hashed).
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

Which installs report, the five conditions that must all hold before anything is
sent, the environment variables that decide them, why the container image
compiles the transport, and how to turn reporting off:
[analytics-configuration.md](analytics-configuration.md).

## Where it hooks in

| Seam | File | Why there |
|---|---|---|
| `turn_finished` | `runtime::cycle::CycleRunner::run_bracketed` | The cycle's whole span, including the wait on the per-company serial lock — which is the part an operator experiences as "nothing is happening". |
| `turn_metered` | `analytics::meter::TrackingUsageMeter`, a decorator over the `UsageMeter` port | Every `metering::record_*` path ends there, on every build. The harness cost hook is richer but `openhuman`-gated, and the cycle-level path deliberately reports zero tokens on that build so spend is not double-counted — so an event at either one is blind on the other half of the fleet. |
| `approval_parked` | **three** park paths: `runtime::cycle`'s park path, `CompanyRuntime::park_blocker` (a planning blocker), `workflows::delivery::park_and_journal` (a gated delivery) | One emission per path, each **after** that path's journal write commits, so a park that was rolled back reports nothing. This row used to claim the cycle was the only write into the operator's queue; it is only the *cycle's*, and the claim is how `park_blocker` came to be forgotten (review of #1950) — every planning blocker an operator later decided produced an `approval_decided` with no park behind it. |
| `approval_decided` | `runtime::cycle::decision_event` for `approved`/`denied`; `CompanyRuntime::retire_approval` for `expired` | Both settle paths — plain and amended — build the event from state read before `resolve_outcome` consumes the parked entry, and track it **after** `record_resolved` commits: a decision that failed to journal has not happened, and boot replays the approval as pending, so reporting at the attempt counts one approval twice. `Expired` returns early here and is deliberately **not** reported from this seam; the settle owes the caller a `retire_approval`, which is the single retirement primitive the TTL sweep reaches by the same path — so both the late click and the sweep arrive at one line, after `record_expired` commits, and only for `ExpiryReason::Ttl`. A `CardUnwritable` rollback is a store fault, not a deadline, and reports no verdict at all. |
| `tool_called` | `harness::built_in::steps::track_tool_call`, from the per-turn progress collector | The collector hands it **every** progress event, so it carries no decision of its own — which events count, and what each may say, is decided in one testable place. |
| `workflow_run_finished` | `workflows::runner`, at the `WorkflowRunner` port boundary | The one place every run — console, scheduler, an orchestrator's `run_workflow` tool — passes exactly once. Timed from after the roster warms: charging a cold start to the first run makes it look pathological beside every later one. A warm that *fails* is still reported, at zero nodes and zero duration. |
| `ledger_appended` | `ledger::analytics::track_append`, from `company::ledgers` | After the append, so a refused write is not reported as one that happened, and before the fold, so a republish failure cannot lose the count. The tracker reaches **both** ledger contexts — the routes', built from a whole `CompanyRuntime`, and the agent tools', built per turn in `harness::built_in::build::build_agent` — because agent-authored writes are the primary ledger path and reporting only the routes' would have read as "operators write ledgers, agents do not". |
| `connection_changed` | `server::ops::connections::track_connection`, from the connections, MCP and Composio routes | After every removal or write half has landed, so a 409 or a 404 is never reported as a disconnect that happened. An MCP mutation whose probe comes back `Error` reports `failed` rather than the transition it attempted. The Composio token route classifies from the credential state **either side** of the write (`token_transition`), so a rotation is `refreshed` rather than a second new connection and a clear over nothing reports nothing at all. |
| `console_viewed` | `server::ops::console_view` — raised by the **console** | The host cannot see a hash change. See below. |
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

**A 2xx is not an acceptance.** Mixpanel's Track API is non-strict: it answers
`200` to a refusal and puts the reason in the body — `1` or `{"status": 1}` for
accepted, `0` or an `error` key for refused. Reading the status alone therefore
counted an invalid token as a delivery, and a misconfigured project dropped
every batch while the transport's "the collector refused" line never fired once.
The body is the only thing that actually says, so the transport reads it.

An unfamiliar answer — a proxy returning its own JSON, an empty body — is given
the benefit of the doubt. This exists to catch a **stated** refusal; treating
every 2xx it does not recognise as a failure would fill a working setup's log.

What the log records is a **classified verdict** — `refused`, `status-zero`,
`error`, `unparsed` — and never the body. The body is the collector's own text
and can carry the event payload back, so printing it verbatim would put in a log
line exactly what the vocabulary in this document spends itself avoiding.

### Console views come from the console

`console_viewed` is the one event in this set the host does not raise, because
the host cannot see it: the console is a single-page app, so moving between
pages is a hash change and no request reaches the process. Without a route,
"which surfaces do operators actually use" is a question the product cannot
answer about itself. `POST /api/v1/companies/{id}/analytics/console-view`
(`src/server/ops/console_view.rs`) is that route — scoped and authorized like
every other company route, because recording which page someone opened must not
become a way to ask whether a company exists. It stores nothing, returns nothing
and answers `204`.

**The view, never the hash.** `#/chat/dm:ada-1f3k` names a teammate,
`#/tasks/<uuid>` names a task and `#/ledgers/<slug>` names a business record —
all of them the company's own content. So the body is one field, the console
sends the routed view alone, and the second segment is not accepted, not
trimmed, not read. `AppShell` keys its report on `view` and not on `sub` for the
same reason, and because re-firing per sub-page would count opening one task as
visiting Tasks twice.

Both sides fold onto a closed list — `src/analytics/console.rs` on the host,
`frontend/src/lib/console-routes.ts` in the console — and neither trusts the
other. The console's `View` union means it cannot be handed a hash by accident;
the host folds what arrives again anyway, because it arrived over HTTP from a
client this crate does not control. Anything off the list becomes `other`,
which is why the two copies have to stay in step: a view added to the console
and missed on the host reports as `other`, and that reads as "operators do not
use that page".

The route is registered in **every** build rather than behind the cargo feature.
A host with analytics off drops the call into the null tracker, and
`Tracker::track` is synchronous and infallible, so navigation is never slowed by
telemetry; the console's own call is fire-and-forget with a swallowed rejection,
because an operator must never see a toast or a blocked render because a
telemetry write failed.

## What is deliberately not instrumented yet

Named so the gaps are countable rather than implied. Each is a follow-up, not an
oversight:

- a timed flush shared with `MaintenanceTicker` rather than the transport's own
  30-second loop.

The other two entries this section carried are gone because they landed on
2026-08-29, not because the list was tidied: the six surfaces #1739 named as
candidates — approvals, tools, workflows, ledgers, connections/MCP and console
views — are the seven events in [What is collected](#what-is-collected), and the
**build commit** is on the envelope.

## Testing

| Lane | Command |
|---|---|
| default build — the decision, the vocabulary, every classifier, the payload builder, the meter decorator, and each of the ten seams | `cargo test --locked` |
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

Two more in the gated lane guard the transport's own two decisions:
`the_send_url_turns_geolocation_off` asserts `ip=0` is on the posted URL and
that a collector which already set `ip` is left alone, and
`a_two_hundred_can_still_be_a_refusal` asserts both that a refusal body is
classified as one and that the verdict never echoes it.

Each classifier is tested beside the code it protects, and the leak guards there
are built to one shape — `ledger::analytics`, `server::ops::connections` and
`analytics::console` each carry one. Two obvious
assertions (a known input keeps its name; an unknown one becomes `other`) are
not enough on their own: a classifier that simply echoed its argument would pass
both on every known value and leak on every unknown one. So each guard pushes a
distinctive needle through the only door the caller has, asserts it is absent
from the rendered payload, and then **self-checks** that the same
case-insensitive search does find that needle in a rendering which carries it.
Without the second half, an assertion passes because the needle was unfindable
rather than because it was absent, which is how a redaction test rots into a
test of nothing.
