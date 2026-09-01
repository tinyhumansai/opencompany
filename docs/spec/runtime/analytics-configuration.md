# Product analytics — configuration

**Status: implemented (issue #1739).** Which installs report, the five
conditions that must all hold before anything is sent, the environment variables
that decide them, and how to turn reporting off. What is actually collected, and
what never is, is in [analytics.md](analytics.md).

## The switches

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

Condition 1 is met in two places, both of them the container image: the
`ARG FEATURES="analytics"` default in `Dockerfile`, and `TENANT_FEATURES` in
`.github/workflows/deploy-staging.yml`, which passes the hosted tenant's full
feature set as a build arg and overrides that default. Nothing else compiles the
feature — not the desktop (`src-tauri/Cargo.toml`), not `cargo build` with no
`--features`, not any CI lane but the scoped analytics one.

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

## Why the image compiles the transport

The `Dockerfile` default was empty until 2026-08-29, and it made the promise at
the top of the [main document](analytics.md) stronger by making the hosted image
itself unreliable.

Condition 1 is the only one of the five that cannot be satisfied at runtime, and
a build that misses it fails **silently and permanently**: the manager injects a
token, `resolve` returns `Report`, `mixpanel::build` hands back a `NullTracker`
because there is no transport in the binary to hand back, and the one line
explaining that is a `tracing::info!` the CLI's default `EnvFilter` swallows.
Boot says nothing is wrong, the dashboard stays empty forever, and nothing
anywhere says why. Every other condition announces itself in a boot line an
operator can read.

So the default moved into the artifact that *is* the hosted workload, rather
than living only in a CI variable that a differently-built image quietly misses.
**Compiling the transport in is not reporting**: the other four conditions are
unchanged and every one of them is a runtime decision. What changes is the kind
of guarantee this one artifact carries — a **will not** where it used to be a
**cannot** — and that is the trade, made deliberately, in exchange for the
failure above becoming impossible. The desktop build, which is where the
stronger promise is made and kept, still compiles no transport at all.

## How to turn it off

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

## Tenant identity is keyed, not merely hashed

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
