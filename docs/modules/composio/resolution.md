# Which credential a call presents

The resolution rule, the routing surprise it contains, and the boolean that has
already misled one console panel. This is the file to read before building
anything that reports whether Composio works.

Everything here is implemented in `src/company/composio.rs`.

## Two questions, deliberately not one field

- **Which host** do the calls go to? That is `ComposioMode` — `managed` or
  `byok`. It is stored under `composio/mode`.
- **Whose identity** do they present? That is `CredentialSource` — `attested`,
  `company`, `static` or `none`.

They are orthogonal and the DTO keeps them apart. A BYOK company reports
`byok` + `static`; a company that pasted a backend token override reports
`managed` + `static`. Collapsing them into one field leaves the console unable to
say which of the two an operator is looking at.

## The managed chain

Under `managed`, `resolve_credential` answers in this order, first hit wins:

```
  1. composio/token          the company's own backend bearer   → static
  2. the company's TinyHumans key                               → company
  3. this instance's platform identity                          → attested
  4. nothing                                                    → none
```

Steps 2 and 3 come from the shared brokered-credential seam
(`company_key::resolve`), so a rotated company key reaches Composio in the same
cycle it reaches every other brokered surface.

**Steps 2 and 3 are not collapsed.** One bills the company's own TinyHumans
account and the other bills whoever runs the server. That is the decision an
operator is on this page to make, so the row names which of the two answered.

A store read error **propagates** rather than degrading to the next tier. An
unreadable store must not be able to silently change which account a call is
attributed to.

## The BYOK short-circuit — the part that matters

Under `byok`, `resolve_access` reads `composio/api_key` **and nothing else**. The
managed chain is skipped entirely for the acting credential.

It deliberately does **not** fall back to the managed tiers when the key is
missing or blank. `Credential::None` there means no tools this cycle, which is
the same fail-closed answer an absent managed credential gets. A company that
asked to act through its own Composio account and silently acted through the
platform's instead would connect providers into the wrong tenant and bill the
wrong party.

The mirror image is just as important and is easier to miss: **managed
resolution reads `composio/token` and the company key, never `composio/api_key`.**
So a stored BYOK key in managed mode simply goes unread. It does not leak and it
is not presented anywhere — it is inert. The code's own comment calls this "the
routing surprise", and names it as the part that matters.

The console's job is to make that visible rather than silent. Both rows are
rendered at all times, each reporting its own state, so an operator can see that
the key they pasted is stored and *not in use* rather than discovering it by
wondering why nothing changed.

### Writes are ordered by direction, not by a fixed key-then-mode

`store_api_key` writes the key and the mode together, and picks the order so that
a **failed second write leaves an inert state rather than an outage**:

- **Selecting BYOK writes the key first.** If the mode write then fails, the
  company is still `managed` holding an unread key — inert, because managed
  resolution never looks at `composio/api_key`.
- **Clearing writes the mode first.** A fixed key-then-mode order would write the
  *empty* key first; if the mode write then failed the company would stay `byok`
  with an empty key, which resolves to `Credential::None` — the exact "BYOK mode,
  no key" outage, reached from the other direction.

## The trap: `token_configured` is not "does Composio work" (issue #886)

`token_configured` answers exactly one question about exactly one slot: **did
somebody paste a token into `composio/token`.** That is the *first* tier of
three.

On a hosted tenant nobody pastes one — the third tier answers, from the
instance's platform identity. So `false` from that function is **routinely true
of a company whose Composio tools are wired and working.**

That is not hypothetical. Issue #886 was filed because the capabilities panel
reported `composioTokenConfigured: false` while agents were calling `GITHUB_*`
tools successfully in the same session.

**Do not build any "is this working" surface on that boolean.** Ask the resolver:
`resolve_access()` → `Credential::source()` for the tier, `configured()` for the
boolean. That is the same derivation the toolbelt gates on, so it cannot disagree
with what the agents actually hold.

The status route already does this correctly — `credentialSource` comes from
`resolve_access`, and `token_configured` was removed from that module's import
list deliberately, with a comment saying so. The read DTO carries **no** `token`
and **no** `tokenConfigured`, and tests assert their absence. Keep them absent.

Use `token_configured` only where the BYO slot itself is the subject — a field
that says whether *this company pasted a token*, never whether it has one.

## The gap this change closes: `managedCredentialSource`

There is one thing the old shape could not express.

Under BYOK, `resolve_access` short-circuits, so `credentialSource` reports the
**BYOK key's** source. The managed row therefore had no way to say which tier
*would* answer if the company switched back — and "switch to managed" with
nothing resolving behind it is an outage dressed as a choice.

So the status DTO gains one **additive, non-secret** field:

```
managedCredentialSource: "attested" | "company" | "static" | "none"
```

It is the managed chain's answer, derived **independently of the stored mode**.
Under `managed` it equals `credentialSource`; under `byok` it names what managed
would resolve to.

It is a **tier name**. Never a credential, never a path, and never a boolean
about a secret slot — reintroducing a `tokenConfigured`-shaped field here would
walk straight back into #886.

### It is not free, and an earlier plan said it was

An earlier version of this plan assumed the managed chain was "resolved anyway as
the catalog bearer", so the field would cost nothing.

**That is wrong, and the code says so.** `fetch_catalog` calls `resolve_tenant`,
which calls `resolve_access` — so under BYOK the catalog is fetched with the
**BYOK key** against `backend.composio.dev`, not with a managed bearer. There is
no managed resolution happening on that path to piggyback on.

Reporting the managed tier therefore requires an **additional**
`resolve_credential` call in the status handler. That is cheap — secret-store
reads, no network — but it is a real extra derivation and it is recorded here so
nobody re-derives the false version of it.

## What the rows say

The managed row's sub-line is a pure function of `managedCredentialSource`:

| `managedCredentialSource` | Sub-line |
|---|---|
| `static` | Using the Composio token saved for this company |
| `company` | Billed to this company's TinyHumans account |
| `attested` | Billed to whoever runs this server |
| `none` | No credential resolves — agents cannot connect apps |

And the control follows from it: when it is `none`, the managed row offers **no**
`Use this`. Switching to a route that resolves to nothing is not a choice an
operator should be able to make by accident; the row says why instead, and points
at the credential card that would fix it.

The BYOK row:

| State | Sub-line |
|---|---|
| active, key stored | `•••• configured · backend.composio.dev` |
| active, no key | amber: BYOK is selected with no key stored — tools are withheld |
| not active | Not configured |

The "active, no key" state is real and reachable, not defensive padding:
`resolve_access` logs a warning for exactly it, and the harness withholds tools
for that cycle.
