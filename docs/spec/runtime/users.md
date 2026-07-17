# Human users

Each company has its own directory of **users**: the humans who collaborate
with its agents through chat. Users are not billing subjects — the platform's
Node backend owns accounts and money, and nothing here knows about either. A
user exists inside exactly one company.

This is distinct from, and weaker than, the machine credentials in
[config.md](config.md): a user is a collaborator, never an operator.

## The shape

| Concern | Answer |
|---|---|
| Sign in | Magic link (256-bit token, 15-minute TTL, single use), **or** an optional password |
| Session | Opaque 256-bit token in an `HttpOnly; SameSite=Lax; Path=/` cookie, 14-day absolute TTL |
| Access | **Invite-only.** An uninvited address cannot log in |
| Bootstrap | The manifest's `[users] admins` list |
| Roles | `admin` (may invite and administer) / `member` |

## Storage

Three ports, all keyed by `CompanyId` like every other:

- `UserStore` — users and invites (they share the email keyspace, so "invited"
  and "joined" are two states of one address and must stay consistent).
- `SessionStore` — live sessions, looked up by token hash.
- `LoginCodeStore` — pending magic-link codes.

Sessions and codes are **credential material**, which is why they are separate
from the directory: they carry their own expiry/purge rules and must never join
the export path (`opencompany export` covers company/event/memory/context
only — do not add them).

**Only hashes are stored.** The plaintext of a session token or login code
exists in exactly one place — the browser's cookie jar, or the email that was
sent — and is never written down. Lookup is *by* the hash: the presented secret
is hashed and that is what's queried. Nothing compares a secret, which is why
there is no constant-time comparison anywhere in this flow. Forging a hit would
need a SHA-256 preimage.

Passwords are Argon2id in PHC format, so each hash carries the parameters it
was made with and old hashes keep verifying after the cost is raised.

## Two isolation guarantees

A session for company A is refused for company B **twice over**:

1. **The storage partition.** `SessionStore::find_by_token_hash` takes a
   `CompanyId`; A's row simply is not in B's partition. There is no filter to
   forget — the conformance suite asserts this on all three backends.
2. **The principal check.** `GqlAuth::authorize` compares
   `UserPrincipal.company`. Belt and braces, in case a cache ever bypasses (1).

The cookie is named `oc_session_<company>` rather than a constant. Hosted mode
serves one company per container, but local development serves many from one
origin, where a constant name would mean signing into B silently destroys your
session for A. The name also lets the GraphQL handler find the company, whose
query argument lives in the request body and is unavailable to extractors.

A company id that cannot safely name a cookie (anything outside
`[A-Za-z0-9_-]`) cannot hold a session: `CompanyId::new` validates nothing, and
`evil;Path=/` would otherwise choose the cookie's attributes.

## Users cannot become operators

`resolve_claims` reads machine credentials and **cannot return
`GqlAuth::User`**. Every operator/platform write route resolves through it, so
a session cookie is unreachable on the write plane — not because each route
checks, but because the type it receives cannot represent a human. Routes that
mean to serve humans opt in by calling `resolve_principal` instead; today that
is the login routes, the admin routes, and chat.

This matters more than it looks. The REST extractors flatten `Dev | Operator`
into `Some(Self(None))`, and `authorize_address` reads `None` claims as *allow
everything*. A user mapped onto `None` would silently have become an operator.

## Bootstrap: `[users] admins`

Access is invite-only, so someone must send the first invite — and there is no
operator token to do it with ([config.md](config.md) explains why). The company
manifest is the root of trust:

```toml
[users]
admins = ["ada@example.com"]
```

Listing an address does not create an account; it makes the address *eligible*.
Redeeming a link mints the user as an admin. Removing an address stops it
bootstrapping again but does not delete an account it already created — use the
admin routes.

Manifest admins appear in the invite list as synthetic `manifest:` entries.
Revoking one is refused: the manifest would re-grant it on the next login, so
succeeding would be a lie.

## Routes

Login routes are **unauthenticated by construction** (`PublicCompany`), because
asking for a link is what someone does when they have no credential. Both
addressing forms work: `/api/v1/companies/{id}/…` and `/api/v1/company/…`.

| Route | Purpose |
|---|---|
| `POST …/auth/request` | Mail a magic link. Always `{"sent": true}` |
| `POST …/auth/verify` | Redeem a link → session cookie |
| `POST …/auth/login` | Email + password → session cookie |
| `POST …/auth/password` | Set/replace your own password (needs a session) |
| `GET …/auth/me` | Who this session belongs to |
| `POST …/auth/logout` | Revoke this session |
| `GET …/users` | The roster (admin) |
| `GET/POST …/users/invites` | List/send invites (admin) |
| `DELETE …/users/invites/{id}` | Revoke an invite (admin) |
| `PATCH …/users/{id}` | Role, status, display name (admin) |
| `POST …/users/{id}/password` | Set a temporary password (admin) |
| `DELETE …/users/{id}/sessions` | Sign a user out everywhere (admin) |

### Every login failure is identical

`auth/request` always returns `{"sent": true}`. `auth/verify` and `auth/login`
always fail with one `401 invalid_login` — for unknown address, uninvited
address, expired code, spent code, wrong code, wrong password, no password set,
and suspended user alike.

This is deliberate. Any difference turns these routes into a **membership
oracle**: someone who can ask "is bob@acme.com a user of this company?" learns
the org chart, and every answer is a phishing target. It is also why
`password::dummy_verify` burns equivalent work where there is no hash to check
— response *time* would otherwise answer what the body refuses to.

Clients must not undo this. The console renders one vague message.

## Passwords

Optional. A user may set one to skip the round trip through their mailbox; a
user who never does is unaffected (`password_hash` is `None`).

There is **no password-reset credential**. "Forgot my password" is a magic-link
login followed by setting a new one — reusing a path that already exists rather
than adding a second emailed secret to get wrong.

An admin may instead set a **temporary password**, which revokes the user's
sessions and pending codes and sets `mustChangePassword`. Note two things:

- The admin knows that password and must convey it out-of-band. That is
  inherent to the option, not a defect.
- `mustChangePassword` is **advisory**: it is surfaced to the console, which
  drives the change. It is not a per-route boundary — the user is authenticated
  either way — so it constrains a cooperating client, not an adversarial one.

Policy is length-only (12–512 characters, counted as characters), with no
composition rules: NIST SP 800-63B recommends against them, as they produce
`Password1!` and buy no entropy.

## Revocation

The user record is re-read on **every** authenticated request, so suspending or
removing someone takes effect immediately rather than whenever their cookie
happens to expire. That costs a second store read per request — on the fs
backend, a whole-file read. Use sqlite or mongodb for anything with real users.

Changing your own password revokes every *other* session but keeps the current
one: it is what you do when you think a session is stolen.

## Chat attribution

`CompanyEvent::OperatorMessage` carries `by: Option<Actor>`.

- `Some(Actor { kind: User, id })` — a signed-in human.
- `None` — an operator/platform/dev credential, or an event journaled before
  attribution existed. Both read as `"operator"`; there is no person to name.

`ActorKind::User` is fieldless and the id rides on `Actor.id`, because
`ActorKind` is `Copy` and a `String`-carrying variant would take that away from
every existing holder.

`serde(default)` + `skip_serializing_if` mean every already-persisted event
loads and an unattributed event serializes byte-for-byte as before — no
migration, no stored record touched.

`mine` is per-viewer, so `MessageGql::project` takes a `Viewer`. Authors render
as a display name or the email's **local part**, never the full address: a desk
history is read by every member and should not hand each of them everyone
else's email.

## Mail

Login mail uses the host-level provider (`OPENCOMPANY_MAIL_*`, see
[config.md](config.md)) — a login link is sent on the platform's behalf, not
the company's. With no transport configured, `auth/request` returns the code in
a `dev_code` field and logs a warning, so local development works; a host that
can send mail never echoes it.

## Known gaps

- **No resend throttle** on `auth/request`. An invited address can be mailed
  repeatedly as a nuisance. Not an account-takeover path — each link
  invalidates the last, and only the mailbox owner can read them — and it needs
  an invited address to aim at. Throttling needs a lookup-by-email on
  `LoginCodeStore`, a port change across three backends.
- **No CORS**, so cross-origin dev (`?api=…` from a Vite server on another
  port) cannot carry the session cookie. Use the Vite proxy.
- **Dev mode is still open**: with no `operator_token` — which cannot currently
  be set at all — every operator route allows every request. User auth does not
  change this. See [config.md](config.md).
