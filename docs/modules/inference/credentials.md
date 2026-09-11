# Credential resolution

Which credential a call presents, and why the answer differs per surface today.

This is the design decision the provider rework depends on: `inference/key`
cannot mean two things at once, and today it does.

## What is stored

Three per-company slots in the `SecretStore`, plus one per-process identity from
the environment. All four are write-only over HTTP — no route returns any value,
and each has a test pinning that.

| Slot | Set by | Means |
|---|---|---|
| `composio/token` | `PUT {scope}/composio/token`, admin | a Composio credential this company pasted |
| `tinyhumans/key` | `PUT {scope}/credential` or the link flow, admin | **this company's TinyHumans account** — an identity |
| `inference/key` | `PUT {scope}/inference`, the setup wizard, the link flow | whatever the declared provider wants — a vendor credential |
| env | the deployer, once per process | `TINYHUMANS_TOKEN_FILE` (a path, rotated in place) or `TINYHUMANS_API_KEY` (a value) |

The manifest can only ever *name* a slot (`[inference].api_key_secret`), never
hold a value, and validation rejects a value there that looks like a pasted
credential.

### The fifth place a credential can hide: the endpoint

A URL can carry userinfo — `http://alice:hunter2@127.0.0.1:8597/v1` — and a
`base_url` is none of the things that make the four slots above safe. It is
stored as written, it is returned by `GET {scope}/inference`, which is
`ScopedCompany` rather than admin, so every console reader receives it on every
page load, and it is interpolated into operator-facing failure text. A password
in an endpoint is therefore a password in all three at once.

Two independent mechanisms, because they cover different populations:

- **Refused wherever an endpoint is accepted.**
  `catalogue::endpoint_has_credentials` gates `normalize_local_endpoint`, which
  every stored endpoint passes through, and `validate_parts`, which is the
  manifest and console-`PUT` half of the same rule. It is the same class of
  rule as the `api_key_secret` check beside it: a credential belongs in the
  write-only slot, not in a field that is read back. The draft probe refuses
  one too, rather than putting a basic-auth credential on the wire.
- **Redacted wherever an endpoint is said.** `catalogue::redact_endpoint`
  replaces the userinfo with `***`. Refusal cannot reach a value stored before
  the rule existed, or one arriving from a `company.toml` or
  `OPENCOMPANY_INFERENCE_URL` this host does not own — so every DTO field,
  operator-facing message and log line that names an endpoint goes through it.
  The endpoint used to *make* the request does not, which is the whole
  distinction between the two call sites.

The subtle half is the failure text. `reqwest` already masks userinfo in its own
`Display`, so an error that quotes the upstream string looks safe — and then a
`format!` that adds our copy of the URL beside it puts the credential straight
back. Three separate messages did exactly that.

Strict manifest `validate()` runs only on a first boot with no persisted record
(`src/runtime/builder.rs`), so the refusal cannot brick a company that is
already running on such an endpoint. That company is covered by redaction
instead, and its next edit of the endpoint is refused with a sentence saying
where to put the credential.

## How resolution worked before this rework

**Superseded — kept because the defect it describes is why the chain exists.**
The section below is the *old* behaviour. The shipped chain is
[the target](#the-target--built-not-future-work), and it is built: `tinyhumans/key`
**is** read by inference now, in `managed_identity` (`src/company/inference.rs`),
which is the whole point of the convergence.

An earlier revision of this file asserted the opposite — "`tinyhumans/key` is
never read by inference, verified by grep" — and stayed that way after the
behaviour changed, under a heading that said "today". A reader met the wrong
answer first and the right one sixty lines later. That is worse than no document:
a stale claim carrying its own evidence is one nobody re-checks.

```
COMPOSIO                                  INFERENCE  (before)
  composio/token                            inference/config + inference/key
        │ absent                                  │ absent
        ▼                                         ▼
  company_key::resolve                      manifest [inference]
     ├─ tinyhumans/key      ← consulted          │ absent
     └─ instance identity                        ▼
        │ absent                            EnvDefault
        ▼                                     ├─ OPENCOMPANY_INFERENCE_KEY
  Credential::None → no tools                 └─ instance identity
                                                 │ absent
                                                 ▼
                                            None → echo brain
```

The two columns never met. A company key set in the console reached its *app
connections* and no chat completion, and there was no path by which it could.

### Three consequences of that gap

**Billing splits silently.** *(Fixed by the chain below.)* A company that sets its own TinyHumans key moves
its *app connections* onto its own account and leaves *every agent turn* on
whoever runs the server. Nothing on screen says so, and thinking is the expensive
half.

**The one place the slots met was a copy, not a resolution.** *(Fixed.)*
`POST {scope}/credential/link/finish` used to write the hub-minted key into
`tinyhumans/key` **and** into `inference/key`. It looked like one key serving
both — but rotating the account key afterwards through the normal route did not
update the inference copy, which kept presenting the old value until somebody
replaced it separately. The copy is gone; step 3 of the chain above does the
same job by resolution, so there is nothing to go stale.

**That copy also misrouted.** *(Fixed.)* `link/finish` stored the key with
`provider: "managed"`, and on resolve `normalize_provider("managed")` yielded
`"openrouter"` *before* `is_managed_choice` was consulted — so with a key
present both managed branches were skipped and the endpoint resolved to
`https://openrouter.ai/api/v1`, presenting a `th_…` key as a bearer to
OpenRouter. `resolve_effective_scoped` now passes the **raw** kind to
`resolve_endpoint`, which consults `is_managed_choice` first, and the test for
it asserts the `base_url` as well as the bearer. Checking the bearer alone is
how the bug shipped.

## The target — **built**, not future work

One identity, two surfaces, an optional vendor override above each — and a
provider check that stops an identity reaching a vendor it means nothing to.

This is the chain the code now resolves (`company::inference::resolve_effective`,
`managed_identity`):

```
INFERENCE
  1. provider/<slug>/key    ← a key pasted for this provider. For the managed
                              provider the slug is `tinyhumans`.
  2. inference/key          ← the legacy flat address, READ-ONLY. Same meaning
                              as (1) at an older address; retires itself, see
                              "Convergence" below.
  3. tinyhumans/key         ← this company's account identity. ONLY when the
                              provider is the managed/TinyHumans one.
  4. instance identity      ← TINYHUMANS_TOKEN_FILE, else TINYHUMANS_API_KEY.
                              Same condition as (3).
  5. nothing → agents cannot think, and the banner says which

COMPOSIO
  1. composio/token                             ← a pasted Composio credential
  2. tinyhumans/key                             ← this company's account
  3. the instance identity
  4. nothing → no app tools
```

### Convergence, not migration

Every provider's credential is **written** to `provider/<slug>/key`, the managed
one included, and the legacy `inference/key` is **cleared in the same write**.
Reads try the new address first and fall back to the old one.

An existing company keeps working untouched; the first save of that provider
moves its key and retires the old slot. There is no flag day and no
half-migrated state on a store with no transaction — and the fallback is one
readable line that can be deleted once nothing reaches it.

The store has no delete, so "clear" is a write of the empty string. It has to be
**issued** rather than inferred, and a failure is logged loudly: a key left at
the old address after the new one is written is an orphaned secret, which is an
incident shape rather than untidiness.

Note what is *not* moving: `tinyhumans/key` — the company **identity** slot in
`src/company/company_key.rs` — stays exactly where it is. It is a different
thing from a key pasted for inference, which is why they are steps 1 and 3 of
one chain rather than two names for one slot.

### The provider check is the whole safety property

Without it this model hands a TinyHumans key to OpenRouter — which is exactly
the `link/finish` bug above, generalised. With it:

- provider **is** TinyHumans/managed → the account key is the right credential
  for that endpoint, so use it
- provider is **any other vendor** → the account key is meaningless to them, so
  never send it; require a vendor credential or fail closed

Composio needs no equivalent check because Composio is always brokered through
TinyHumans: there is only ever one vendor at the other end.

State it as a rule: **an identity flows to a surface only when the vendor at the
other end is the identity's own vendor.**

### Why `inference/key` must stop meaning two things

Today that one slot holds either "my OpenRouter key" (a vendor credential) or
"my TinyHumans key, used for inference" (an identity). Those have different
lifecycles — a vendor key is replaced when you change vendor; an identity is
rotated and should propagate everywhere it is used. One slot cannot serve both,
which is why the copy exists and why the copy goes stale.

**A slot should be keyed by what the credential *is*, not by which feature
consumes it.** That is the same modelling error the provider list fixes one level
down: one slot per company overwritten on switch, versus one slot per provider.

## What this resolves

**The stranded-key problem disappears without being handled.** Today, setting a
key and then switching provider leaves a credential for the wrong vendor in the
only slot there is, and the first turn fails with a 401 the host explains in a
paragraph:

> A key is stored against the provider selected when it was saved, so a key for
> another vendor fails here even while this card reports one is set.

When an error message compensates for a modelling gap, the model is wrong. With
the credential living **on the provider entry**, switching provider switches
which credential is in play. Nothing is stranded, because nothing was ever shared
between two providers. The paragraph can be deleted rather than reworded.

**And three bugs stop being reachable:**

- rotation staleness — there is no copy to go stale
- the `managed` → `openrouter` misroute — nothing stores an identity in a vendor
  slot, so normalisation has nothing to misroute
- the silent billing split — one account key moves both surfaces, which is what
  an operator expects

## What stays as it is

- **Write-only, structurally.** No `Serialize` on anything holding a credential,
  private field, redacting `Debug`, boolean on the DTO, and the leak test
  asserting on field values across every route.
- **Obtained per request, never captured at boot.** What makes a rotating
  projected token work at all.
- **Fail closed on an unreadable store.** An unreadable store means *we do not
  know who this company is*; treating that as "no key" would attribute a
  connection to the instance's account rather than the company's, invisibly and
  permanently. Availability-degrade and identity-degrade are different decisions
  and only the first is safe to make silently.
- **One resolution seam per identity**, so a rotation reaches every surface in
  the same cycle rather than one at a time.

## Known gaps this design does not address

Named so they are not mistaken for solved:

- **Secrets are stored in plaintext.** `src/store/fs.rs` says so: "Encryption-at-
  rest is a documented follow-up; Phase 1 stores plaintext."
- **The `SecretStore` port has no delete** — `get` and `set` only. Clearing is a
  write of the empty string, so "never set" and "deliberately cleared" are
  indistinguishable and deletion cannot be proven.
- **No audit trail.** Nothing records who set or rotated a credential, or when.
- **No least privilege.** One TinyHumans key is login *and* Composio *and*
  inference; it cannot be scoped down, so a leak is total. Capability-scoped
  tokens are the eventual answer and are out of scope here.
