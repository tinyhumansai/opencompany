# Connecting a search provider

From "Add a provider" to a working row, and the failure handling that is the
actual substance of it. The shape is the inference connect flow's; **the
classifier is not**, and the reason is the most important thing in this file.

## The page

Two cards, and a row is a mark, a name, one sub-line and a control. No
explanatory paragraphs — the page this replaces opened with prose explaining
controls that were visible while they were being read.

```
  Search                                   Connections › API Keys › Search
  Where your teammates search the web.

  ┌────────────────────────────────────────────────────────────────────┐
  │ Search providers                                                   │
  │ Connect a search account of your own.      [ + Add a provider ]    │
  └────────────────────────────────────────────────────────────────────┘

  ┌────────────────────────────────────────────────────────────────────┐
  │ CONNECTED                                                          │
  │ ◼ Managed                                                  ┃ On ┃  │
  │   Metered — up to 100 searches a day                               │
  │ ◼ Brave Search         Default            [ ▮▬ ]             ⋯     │
  │   •••• configured                                                  │
  │ ◼ SearXNG              search.acme.internal                  ⋯     │
  │   Self-hosted instance                                             │
  └────────────────────────────────────────────────────────────────────┘

  Your teammates search through the default provider.
```

The sub-line is one fact chosen by what the row is: `•••• configured` for an
account provider, the instance host for SearXNG, the metering for Managed. The
mark is a **monogram**, not a brand logo — shipping vendors' trademarks opens a
licensing question this feature has no need to open.

Health is silent when nothing has been learnt **and when it is `ok`**: a column
where every healthy row says "ok" spends itself saying nothing and makes the one
unhealthy row harder to find.

The one surviving sentence is the last line, and it earns its place: it is the
only thing on the page that says a connected-but-not-default provider is not
being used. See [`data-model.md`](data-model.md) — "Default" means something
stronger here than it does on the LLM page.

## Two categories, because they ask two different questions

```
┌─ Add a provider ────────────────────────────────────────────┐
│                                                             │
│  Search accounts                                            │
│  Hosted search APIs. You supply an API key.                 │
│  [ Choose a search provider…                            ▾]  │
│                                                             │
│  Self-hosted                                                │
│  Your own SearXNG instance. No account, no key — just the   │
│  address it answers on.                                     │
│  [ Choose a self-hosted instance…                       ▾]  │
│                                                             │
└─────────────────────────────────────────────────────────────┘
```

The inference dialog has three categories and a custom-provider escape hatch.
Here there are two and no escape hatch, because there is no generic search API
to be custom against ([`data-model.md`](data-model.md)). An operator whose
provider is not listed self-hosts SearXNG or stays on managed.

Each list shows **only what is not yet connected** — the page behind the modal
shows the rest, and offering to add something twice is how you get two rows for
one provider. Here that rule is also what keeps the slug set closed, since the
harness dispatches on the slug.

Dropdown items are two lines: the mark, the name, and a **monospace** detail —
the API host for an account provider, `Runs on your own network` for SearXNG.

The select's value stays pinned empty: choosing an item starts a connect flow and
leaves nothing selected, because the connection state lives in the page rather
than in the control.

## The two connect dialogs

```
┌─ Connect Brave Search ──────────────┐  ┌─ Connect SearXNG ──────────────────┐
│ The key is stored on this company   │  │ The address of your own instance.  │
│ and never shown again.              │  │ There is no account and no key.    │
│                                     │  │                                    │
│ API key                             │  │ Instance URL                       │
│ [                               ]   │  │ [ https://search.acme.internal  ]  │
│ Create one at                       │  │ JSON output must be enabled —      │
│ api-dashboard.search.brave.com      │  │ add `json` to `search.formats`.    │
│                                     │  │                                    │
│ Checking costs one search.          │  │            [Cancel]  [Connect]     │
│            [Cancel]  [Connect]      │  └────────────────────────────────────┘
└─────────────────────────────────────┘
```

No Name field and no slug line. The slug is the catalogue slug, there is at most
one row per provider, and a select over one option is a button wearing a costume.

**"Checking costs one search."** No hosted search provider offers a free
credential validator — this was checked against all three providers' own docs.
Brave, Exa and Querit all reject a bad key *before* running a search, so a
failed check is free, but confirming a **good** key requires a real, billed
query (~$0.005 Brave, ~$0.007 Exa, 1 credit Querit). Inference's probe is a free
`GET /models`; this one is not, and the dialog says so on the button it applies
to rather than above the fold. SearXNG's dialog says nothing, because its probe
is genuinely free.

## The flow

```
  pick ──▶ type ──▶ claim record ──▶ write credential ──▶ PROBE ──┬─▶ ok ──▶ saved
                                                                   │
                                                                   └─▶ classify
                                                                          │
                     ┌────────────────────────────────────────────────────┤
                     ▼                                                    ▼
                reason == auth                                   anything else
                     │                                                    │
       roll back record AND credential                     KEEP the credential,
       reject: "Could not reach X: the provider            keep the row, amber
       rejected the credential."                           advisory on the row
```

Ordering, and none of it is arbitrary:

1. **Validate locally what can be validated locally.** For SearXNG, the URL
   scheme and shape, plus the address guard below. Reject before any write.
2. **Claim the record first, then write the credential.** The claim is a
   test-and-set under the store's per-company index lock: it creates the row
   only if the slug is not already connected, and says which happened.

   This ordering is the fix for a race, and reversing it restores the race. Two
   admins connecting the same provider at once both used to get past a plain
   existence check, and the loser did real damage rather than duplicating work —
   step 4 rolls back by deleting the row **and** the credential, so the request
   whose key was rejected deleted the row and the working key the other had just
   stored, while that one answered `saved: true` from its own request-local copy.

   The credential goes second because a loser must have written nothing by the
   time it is refused; writing it first would overwrite the winner's key at the
   shared address on the way to a 400.

   It does **not** need to land before the probe: the probe is handed the
   request's own key rather than reading it back from the store.
3. **Probe.**
4. **Roll back both stores on a destructive failure**, and log a rollback failure
   rather than swallowing it. A silently failed clear orphans a secret, and in
   this store "cleared" and "never set" are the same state — so the clear must be
   issued, never inferred.

There is no "add anyway" escape. Inference needs one because a provider that does
not serve an OpenAI-shaped `/models` listing is still perfectly usable, so the
probe can fail on a working provider. Here the probe **is** the provider's own
search call, so a probe that fails for anything but a transport reason is a
provider that will not answer a turn either. Offering to skip verification would
be offering to store a credential we have positive evidence does not work.

## The probe is classified per provider, not by one regex

This is where the inference design does **not** transfer, and copying it
faithfully would ship a bug.

Inference classifies on the upstream error *text*, in a fixed branch order:
proxy first, then `401`-or-`403`-with-credential-wording, then model, then
endpoint, then timeout. That works because every OpenAI-compatible endpoint
signals a bad key with 401.

**Brave does not.** A bad Brave key returns **HTTP 422**, with
`{"error":{"code":"SUBSCRIPTION_TOKEN_INVALID", …}}`. Brave's API reference
documents 200, 404, 422 and 429 and **no 401 and no 403 at all**. So a classifier
that maps auth to 401/403:

- never fires the destructive branch for Brave, leaving a dead key stored under
  an amber "the check did not complete" advisory; and
- would, if it ever saw a 403 from that host, classify a **WAF** response as a
  rejected key — the exact incident the inference branch ordering exists to
  prevent, arriving through the opposite door.

So classification is **per provider**, driven by the status and the provider's own
error body:

| Provider | Auth failure | Distinguishing signal |
|---|---|---|
| `brave` | `422` | body `error.code == "SUBSCRIPTION_TOKEN_INVALID"` — **not** the status |
| `exa` | `401` | body `tag == "INVALID_API_KEY"`. A `402` means *either* no header at all *or* out of credit, so classify on the body, not the status |
| `querit` | `401` | body `error_code` — a **string** `"401"` on errors while success carries an **integer** `200`, so deserialize loosely |
| `searxng` | *(none — there is no credential)* | a `403` here means the instance has not enabled JSON output |

Two classes exist here that inference has no need for:

- **`format`** — SearXNG's `403`. JSON output is disabled by default in SearXNG:
  the shipped `settings.yml` has `search.formats: [html]`, and requesting an
  unset format is `flask.abort(403)`. This is neither a credential problem nor a
  WAF, and the copy is the fix: *"Saved. The instance has JSON output turned off
  — add `json` to `search.formats` in its settings.yml."* Classifying it as
  `auth` would delete a credential that does not exist; classifying it as
  `unknown` would waste the one message that could have fixed it.
- **`quota`** — `429` everywhere, and Exa's `402`. Non-destructive.

The generic branches stay, and stay in the inference order, for everything the
per-provider rules do not claim: **proxy/`407`/Cloudflare/bad-gateway first**, so
the word "authentication" inside `407 Proxy Authentication Required` cannot be
read as a rejected key; then endpoint/DNS/refused; then timeout; then `unknown`.
The digit tests use word boundaries so `401`/`403` do not match inside an id.

**Only `auth` deletes the credential.** Everything else keeps it and shows an
advisory, because the credential is plausibly fine and the connection is not.

### Classification is separate from copy

`classify` decides, `describe` says — the same split the inference probe uses,
and for the same reason: the class is unit-testable without a translator, and the
strings stay where strings belong. The console is told the class and chooses its
own sentence; **the raw upstream string never reaches the console**, because it
can echo request material including key fragments and it lands in a banner
somebody screenshots into a ticket.

## What the operator sees

| Class | Credential | Row | Copy |
|---|---|---|---|
| `auth` | deleted | not created | "Could not reach *X*: the provider rejected the credential." |
| `format` | n/a | created, advisory | "Saved. The instance has JSON output turned off." |
| `quota` | kept | created, advisory | "Saved. The account is out of credit." |
| `endpoint` | kept | created, advisory | "Saved, but nothing answered at *host*." |
| `timeout` | kept | created, advisory | "Saved, but *host* did not answer in time." |
| `unknown` | kept | created, advisory | "Saved, but the check did not complete." |

Amber and dismissible, keyed by slug. The save succeeded; only reachability is in
question, and colouring it as an error would be a lie about what happened.

## Probing an instance address, and the SSRF question

A SearXNG instance URL is an operator-supplied address the host will fetch, which
makes this an authenticated "fetch an arbitrary URL" primitive.

The repo already has a guard — and **it turned out not to be reusable**, which is
worth recording because the plan said it would be. `guard_link` in
`src/server/ops/memory_ingest.rs` refuses loopback, link-local *and every RFC1918
address*, plus `.internal` hostnames, re-checking after DNS resolution. That is
right for fetching a link an operator pasted from the internet and wrong here:
**a SearXNG instance is legitimately on a private network**, and
`search.acme.internal` at `10.0.0.5` is the *ordinary* deployment. Calling it
would refuse the normal case. It is also `#[cfg(feature = "documents")]`, and this
surface is deliberately ungated.

So `search::probe::guard_instance_url` is a narrower rule rather than a copy, and
the difference is the point: it refuses only the class that is never a search
instance and is always somebody's cloud metadata service. That makes this the
third URL-shape rule in the tree, and the second one's own comment already says
the lasting fix is to lift it somewhere both can depend on — which is a separate
change, because three callers want three different address policies, so lifting
means parameterising rather than moving.

The allowance is explicit and narrow:

- `http`/`https` only.
- Link-local and cloud-metadata addresses (`169.254.0.0/16` and friends) are
  refused outright, re-checked after resolution. A search instance is never
  there.
- Private ranges and loopback are **allowed**, as a deliberate hole rather than
  an oversight, and this is the reason the probe route is `AdminScopedCompany`
  and not `ScopedCompany` — see below.
- Redirects are not followed **at all** (`redirect::Policy::none()`): a redirect
  to somewhere else is not this provider answering, and following one is how a
  guarded address gets reached anyway.
- Body size and time are capped (4 KiB, 15s); the body is read only for
  classification and never returned.

## Authority

The existing search routes are the model and they are already right: `GET` is
`ScopedCompany`, every write is `AdminScopedCompany`, on the argument that a
search key is billed to whoever's account it belongs to and the provider choice
decides which index — and which retention policy — every agent's queries are
handed to.

The probe route **diverges from inference here**: inference allows `ScopedCompany`
on probe, but this probe spends the company's money (a real billed search) and,
for SearXNG, fetches an operator-supplied private-network address. Both make it a
company-deciding action. **Probe is `AdminScopedCompany`.**

## Testing a draft

`POST …/search/test` must probe a **draft** — a key typed but not stored — as well
as a saved row, since the add flow tests before it commits. That is the same
generalisation the inference plan makes of `POST /api/v1/setup/inference/test`,
under the same rule: testing a credential and committing to it are separate acts.

The credential arrives in the request body and is discarded; it is never written
by the test route, and never returned.
