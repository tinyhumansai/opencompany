# Sign-in modes

How humans prove who they are is **configuration**, not a code path. A company
picks one of three modes, and the one it picks is the only one its host serves.

| Mode | Who signs in | How | Bootstrap list |
|---|---|---|---|
| `email` (default) | an invited address, or a username | password; a magic link where the host mails; the first-admin claim | `[users].admins`, or the first person in |
| `wallet` | an invited base58 wallet | a signed challenge | `[users].wallets` |
| `none` | nobody | there is no sign-in | — |

`email` is the default and is exactly what every company did before this was
configurable, so a manifest that names no mode is unaffected by this existing.
The "email" in the name is the identity key, not a requirement: on a host with
no mail transport a login is a plain username, and nothing is ever sent to it.

Everything downstream of *identification* is shared between `email` and
`wallet`. Both converge on the same `eligibility` → `upsert_from_eligibility` →
`mint_session` path, the same `SessionRecord`, the same cookie, the same roster,
the same revocation. Only the proof differs. That is deliberate: a second way in
that also had a second session model would be two auth systems, and the weaker
one would decide the security of the whole.

`none` does not go through any of that — there is no sign-in proof to check, no
session, no cookie, no invite, and no user-administration flow, because there is
no second person to admit. It reuses the same [`UserRecord`](users.md) shape and
the same audit identity model for its one implicit local owner, materialized on
first request rather than through `eligibility`. See
[`local_owner_record`](#one-identity-column).

## One mode, one door

A mode's routes are the only ones that answer. A wallet company refuses
`auth/request`, `auth/verify`, `auth/login` and `auth/password`; an email company
refuses `auth/wallet/*`; a `none` company refuses all of them.

The refusal is `409 {"code": "auth_mode", "mode": "<mode>"}` — **not** a 404.
The route exists and the request was well-formed; what is wrong is the state of
the company it was aimed at. A 404 is indistinguishable from a version skew or a
typo and sends a client looking for a spelling mistake. For the same reason every
route stays mounted in every mode rather than being left off the router.

Naming the mode in the refusal does not breach the
[generic-failure rule](users.md#every-login-failure-is-identical). That rule
protects *who is on the roster*; the mode is a property of the deployment, and
`GET …/auth/config` publishes it to anonymous callers already — the console
cannot draw a sign-in screen without it.

## Where the mode comes from

The same four layers as every other setting ([config.md](config.md)), earlier
winning:

1. `OPENCOMPANY_AUTH_MODE`
2. `auth_mode` in `config.toml` — what the [first-run setup flow](setup.md)
   writes when an operator picks a sign-in mode. The flow applies the choice
   live by rebuilding registered companies when a rebuilder is available. If
   rebuilding cannot apply the change to a given company, the response reports
   that it needs a restart.
3. the company's `[users].mode`
4. `email`

Layers 1–2 are host-wide and reach every company as
`AppConfig::auth_mode_override`; layer 3 is per company. Both meet in
`RuntimeBuilder::with_auth_mode_override`, which resolves the mode **once, at
build**, and caches it on the `CompanyRuntime`. It is read on the request path —
by the login routes, the user-administration routes, and principal resolution —
and re-reading a manifest per request would be a `CompanyStore` read for a value
that cannot change without a rebuild.

The host layers exist because two deployments own the answer rather than the
company definition does: a hosting platform, which must be able to guarantee a
mode across every tenant whatever a tenant wrote, and any host that must pin a
mode independent of a per-company manifest.

The **packaged desktop app is the canonical layer-1 user.** It sets
`auth_mode_override` to `AuthMode::None` on its own `AppConfig`
(`crates/opencompany-app/src/embedded.rs`), which is what makes a desktop install a host
with no login screen at all. It sets it there rather than writing
`[users].mode = "none"` into the shipped preset manifests, and the difference
is not stylistic: the override reaches *every* company on the data root,
including ones left by an install that predates it — so an existing install
migrates by relaunching — and it leaves `manifest.users.mode` alone, which
matters because `validate_users` flags a manifest carrying `[users].admins`
under `none` and both seeding paths treat a flagged manifest as a hard error.

It is a **default, not a ceiling**: the root's `config.toml` still wins when it
names a mode, so an operator who chooses `email` in setup — to share their
instance with somebody — keeps that choice across a relaunch. See
[the desktop client](desktop.md#no-sign-in-at-all).

An unparseable value **aborts boot**. "The sign-in you configured is not the one
you got" is invisible from a running host.

## One identity column

`UserRecord::email` is the login **identity key**, unique per company, and every
storage backend already indexes it. What fills it follows the mode:

| Mode | Key | Example |
|---|---|---|
| `email` | the normalized address | `ada@example.com` |
| `wallet` | `wallet:` + base58 public key | `wallet:7xKX…` |
| `none` | `local:owner` | `local:owner` |

Built and parsed through `LoginIdentity`, never by hand. Two reasons this is one
column rather than three:

- **The invite keyspace shares it.** "Invited" and "joined" are two states of one
  identity, and suspension, session revocation and removal all key off the
  `UserRecord::id` it resolves to. A parallel column would be a second thing each
  of those paths has to remember.
- **The prefixes make it safe, but only because the parser checks the whole
  scheme, not merely the colon.** `normalize_email` only lowercases and trims —
  an email local part may legally contain a colon, so `wallet:ada@example.com`
  is a normalized key like any other. `LoginIdentity::parse` therefore trusts
  `wallet:` only when the remainder actually decodes as base58, and `local:`
  only when the remainder is exactly `owner`; anything else, including an email
  that happens to start with one of those words, falls through to `Email`. No
  wallet can pose as a mailbox or the reverse. An unprefixed key is an email,
  which is what every record written before this loads as — that is the whole
  migration.

Normalization differs by scheme and the difference is load-bearing:
`normalize_email` folds case, and folding the case of a base58 address would map
two distinct wallets onto one identity, so a signature verified against one would
mint a session for the other. `normalize_wallet` trims and nothing else.

Mail paths must ask `LoginIdentity::mailbox()`, which is `None` for everything
that is not an email. A `wallet:7xKX…` handed to an SMTP transport is a bug that
surfaces only in a bounce log, so the absence of a mailbox is a type rather than
a convention.

## `wallet`

Ed25519 over base58 — the Solana-style address space — verified by
`crypto::ed25519`, the same code the runner handshake uses, so there is one
answer to "does this signature verify" rather than two.

```text
POST …/auth/wallet/challenge  {address}            → {nonce, message, expiresAtMillis}
POST …/auth/wallet/verify     {nonce, signature}   → session cookie + the user
```

The signed bytes, versioned by their first line:

```text
opencompany-wallet-login-v1\n
<company id>\n
<base58 address>\n
<nonce>\n
<issued at, epoch millis>
```

Every field is bound for a reason, and the reasons are the attacks: the **domain
tag** because a wallet signs for many protocols with one key and a signature
gathered elsewhere would otherwise replay as a login; the **company id** because
otherwise a signature for company A logs its holder into company B on a host
serving both; the **address** to bind the proof to the key it claims; the
**nonce** to make each signature good exactly once; the **issue time** to make a
captured message legible as stale. Clients must sign the `message` verbatim
rather than rebuilding the layout, or every future change to it is a breaking
change for every client.

Three properties are worth stating outright:

- **Verification reads the record, never the request.** The address and the issue
  time come from the stored challenge; the caller supplies only the nonce and the
  signature. Rebuilding the message from anything the caller sent would let them
  choose which wallet they are proving control of.
- **Redemption happens before the signature check.** `LoginCodeStore::consume` is
  atomic, so a nonce cannot be spent twice by two racing requests — and a caller
  who could burn attempts against a live nonce without consuming it would have
  unlimited tries at forging one signature.
- **A challenge is not evidence of anything.** Every well-formed address gets
  one, including addresses this company has never heard of; an ineligible one is
  simply never persisted, so answering it fails with the same `invalid_login` a
  forged signature gets. That keeps the route from being a membership oracle
  *and* stops an anonymous caller filling the code table with invented addresses,
  which they otherwise could — a wallet address is public and costs nothing to
  make up.

Challenges live 5 minutes, against a magic link's 15. The difference is not
caution: a link waits in a mailbox for a human to notice it, while a challenge is
answered by software the moment it arrives.

Challenges share `LoginCodeStore` with magic links and device-pairing codes, kept
apart by hashing under a distinct domain prefix rather than by a discriminant
field — three keyspaces cannot collide, whereas one keyspace plus a check is only
as good as every caller remembering the check.

There is **no password path and no mailbox** in a wallet company: no magic link,
no invite mail, no first-admin claim (the key list is the bootstrap). An admin
still
invites a base58 address exactly as they would invite a mailbox; the invitee
learns of it out of band, and the invite reports its delivery as `no_mailbox` —
not a failure and not a missing transport, but the honest statement that there
was never a message to send.

There is deliberately no environment counterpart to `OPENCOMPANY_ADMIN_EMAIL`.
That variable exists because a platform-provisioned company's creator is known
only to the control plane, and the control plane records an email address, not a
wallet.

## `none`

What the packaged desktop app runs: a loopback host, one person, no accounts.

It does not merely hide the login screen. There is no sign-in and **no way to add
a second person** — every route that would invite, re-role, suspend or
re-credential somebody is refused, because a company nobody signs in to has no
way to tell one human from another and an invite would grant an account that can
never be reached. Listing stays available: showing the one local owner is honest,
and an empty screen would not be.

`resolve_principal` answers with that owner **before** the session and bearer
paths, and that order is the point — in `none` mode there is no session to find
and no bearer to verify, so falling through would leave every request
unauthenticated against a host whose whole premise is that its only caller is its
owner.

The owner is a real `UserRecord`, materialized on first use under
`LoginIdentity::Local` and minted as an `Admin`. Not a principal invented per
request: chat attribution, task assignment and the audit trail all key off
`UserRecord::id`, and a synthetic id no store row backs would come apart at the
first of them that looked the user up. It is the one place a user is created
without anyone proving anything, and it can be, because the mode's premise is
that whoever reaches this host is its owner.

**A remotely paired device cannot be used against a `none`-mode host, and this
is a real capability the desktop gave up to get here.** The flow does not fail
where anyone would notice it: the person at the machine is the local owner, so
they can mint a pairing code, and `claim` finds `local:owner` by identity and
redeems it into a genuine device `SessionRecord`. Only the *use* fails, and only
from the one place the credential was minted to be used. Two independent
refusals stand in the way and either alone is enough — `authenticate_session`
returns `None` for any session on a company with no login (the rule that stops a
session outliving a mode flip), and `resolve_principal` asks `local_owner`
first, so a remote device's non-loopback peer answers `GatesRefused` and the
request is refused outright rather than degrading to the session path at all.

That is accepted rather than worked around. Pairing a phone to a laptop's
company is a second person on a second machine, which is exactly the premise
`none` trades away in exchange for having no accounts; a desktop that wants it
should choose `email` in setup, which is why that choice is a preselection and
not a lock. `none_mode_pairs_a_device_that_cannot_then_be_used_remotely` in
`src/server/users/mode_test.rs` pins the whole shape, minting included, so the
trap is documented as behaviour rather than as prose.

**Three independent gates, not one, keep `none` from being reachable from
somewhere its operator did not intend.** A company with no sign-in on a host
anyone can reach is an unauthenticated admin console, not a desktop app, and the
contradiction is otherwise silent — the host does start and does serve. Each
gate catches a different way that could happen, and none of them substitutes
for the others:

1. **Boot- and provision-time: `AppConfig::is_local_only()`.** The same
   predicate that gates echoing a login code in a response. Refuses to boot,
   or to provision via `POST /api/v1/companies`, a `none`-mode company whenever
   the bind is not provably loopback — including a *declared* reverse proxy,
   which sets `OPENCOMPANY_PUBLIC_URL` and so also fails this check. A failure
   aborts exactly as a selected-but-unavailable storage backend does. This is
   the only one of the three that can refuse before the company is ever live,
   and the only one enforced at every runtime-registration path (boot, API
   provisioning, and the desktop app's own loader).
2. **Per request, the TCP peer.** `local_owner` (the resolver behind the
   `none`-mode owner) refuses a non-loopback peer, when the serving path can
   name one — every production listener can, via `axum::serve`'s connect info.
   This catches a directly reachable socket, e.g. a future registration path
   that lands `none` mode on a routable bind some other way.
3. **Per request, proxy-forwarding headers.** `local_owner` also refuses
   `X-Forwarded-For`, `X-Forwarded-Host`, RFC 7239 `Forwarded`, or `X-Real-IP`
   (not RFC 7239, but a common `nginx` recipe sets it). This is the gate the
   peer check cannot be: a same-host reverse proxy connects to a
   loopback-bound listener over loopback too, so the peer this process
   observes always reads as loopback no matter where the proxy's own caller
   was. An *undeclared* proxy that forwards one of these four headers — the
   default behavior of every common reverse proxy — is what this catches.

   **What it does not catch:** a proxy specifically configured to strip all
   four before forwarding is indistinguishable, by header content, from no
   proxy at all. Detecting that case needs a trusted-proxy boundary (a
   configured allow-list of proxy peers whose headers are trusted, and a
   refusal of anyone else's) — a real feature, not a one-line addition, and
   not implemented here. This gate's actual guarantee is narrower than "no
   proxy can bypass it": it catches every reverse proxy using its own
   defaults, and does not catch one an operator has deliberately hardened
   against detection.

When neither of the per-request signals is available (an embedded caller with
no real socket and nothing forwarding for it, or a test), neither refuses on
its own — gate 1 is still the one that ran, and 2 and 3 are additive to it, not
a replacement for it.

## What the console is told

`GET …/auth/config` →
`{"mode": "email"|"wallet"|"none", "name": string, "passwords": bool, "magicLink": bool, "claimable": bool}`.

Unauthenticated by construction, like every other login route: the console asks
before anyone has a credential, because it cannot choose a screen otherwise. It
must branch on this rather than on which routes fail — a wallet company and a
misconfigured email company both refuse `auth/request`, and only one of them
should be offered a wallet button. A console that cannot reach this route assumes
`email`, which is what every host predating it does.

`name` is what the company calls itself — the manifest's display name, falling
back to the company id, never empty. It is here because the sign-in screen is
where a person confirms *what* they are handing a credential to, and every other
route that reports the name is behind the very sign-in being drawn. On the
hosted platform each tenant is a separate company on its own URL, so the host
knows this for certain; publishing it discloses no membership, exactly as the
mode does not. A console talking to a host that omits it draws the bare
"Sign in" it drew before the field existed.

`magicLink` is whether a link asked for here reaches a mailbox: a wired mail
transport, and nothing else. It is false on any host with no transport — the
loopback echo of the code (`dev_code`) is a developer convenience on the API
and deliberately does not count — and the console then draws the password form
alone rather than the link form: `auth/request` answers `sent: true` there
exactly as it does on a host that delivered, deliberately, so nothing about the
response itself tells the person their link went nowhere. A host predating the
field is assumed `true`, matching the `email` default above.

`claimable` is whether nobody has joined this company yet, so the first person
in may pick the admin login and a password (`POST …/auth/claim`,
[users.md](users.md#first-admin)). Only ever true in `email` mode, and only
until the first user exists. A host predating the field is assumed `false` —
it has no claim route to send anyone to.

## Related

- [Human users](users.md) — the roster, sessions, invites, and the rules every
  mode shares.
- [Configuration](config.md) — the precedence layers and the environment.
- [The company manifest](manifest.md) — `[users]` and the rest of the schema.
