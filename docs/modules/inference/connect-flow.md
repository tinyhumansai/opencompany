# Connecting a provider

The flow from "Add provider" to a working entry, and the failure handling that is
the actual substance of it.

## Why a modal

There are a dozen or so providers and a company connects one or two. Listing them
all inline spends the page on the ones nobody chose and buries the ones actually
configured. openhuman's own note on this is worth keeping:

> Custom is deliberately NOT a fourth select. It is one option, and a select over
> one option is a button wearing a costume.

## The page the modal opens from — as shipped

Two cards and one line. The page this replaces opened with four paragraphs —
what bring-your-own-key means, what Test costs, what Reset does, what Remove key
does — above a form, and every one of them explained a control that was visible
while it was being read.

```
  LLM                                        [ LLM Providers | Routing ]
  Configure AI providers, local models, and the agent chat tools.

  ┌────────────────────────────────────────────────────────────────────┐
  │ LLM Providers                                                      │
  │ Add and configure language model providers.   [ + Add a provider ] │
  └────────────────────────────────────────────────────────────────────┘

  ┌────────────────────────────────────────────────────────────────────┐
  │ CONNECTED                                                          │
  │ ◼ Managed                                                 ┃ On ┃    │
  │   TinyHumans chooses a model for each task                         │
  │ ◼ OpenRouter                        Default        [ ▮▬ ]   ⋯      │
  │   •••• configured                                                  │
  └────────────────────────────────────────────────────────────────────┘

  Managed is always available as a fallback. To use your own model, choose a
  routing mode below.
```

A row is **a mark, a name, one sub-line and a control**. The sub-line is one
fact chosen by what the row is: `•••• configured` for a keyed provider, the
endpoint host for a keyless one, `Runs on this machine` for a local runtime.

The mark is a **monogram**, not a brand logo — shipping thirty vendors'
trademarks opens a licensing question this feature has no need to open, and a
missing logo would leave a hole in the row.

Health is silent when nothing has been learnt **and when it is `ok`**. A column
where every healthy row says "ok" spends itself saying nothing and makes the one
row that is not healthy harder to find.

Two explanations survive the deletion pass, and both carry something no control
on the page does: the **restart notice**, because a save that has landed and is
not yet in effect looks exactly like one that is, and the **cost warning** on the
routing dialog's Test — which sits on the button it applies to rather than above
the fold.

## The Managed row tells the truth about itself

The design this ports renders Managed with a permanent `Always on` badge, and
**there that is true** — the same company runs the managed backend. Here the
managed tier needs a credential and can resolve to nothing, so the badge was a
claim of availability the row could not back. That is the failure
`CognitionState`'s five states exist to prevent, on the one row an operator
looks at to answer "can my agents think".

The chain already answers it, so the row reports which step did:

| Resolves at | Badge | Sub-line |
|---|---|---|
| `provider/tinyhumans/key`, or legacy `inference/key` | `On` | Using the key saved for inference |
| `tinyhumans/key` — the company's account | `On` | Billed to this company's TinyHumans account |
| the instance identity | `On` | Billed to whoever runs this server |
| nothing | *(none)* | No credential resolves — agents cannot think |

The last two "on" states are **not collapsed**. One bills the company's own
account and the other bills whoever runs the server, and that is the decision an
operator is on this page to make.

**No toggle on this row.** Managed cannot be switched off, and a control that
does nothing is worse than no control — the same reasoning the read-only rows
already follow.

### Setting it up is the ordinary add flow

The row is present **only while the chain resolves**, and it is keyed on
resolution rather than on a provider record existing — steps 3 and 4 answer from
the company identity and the instance environment, neither of which is a record,
so a hosted tenant has a working managed provider nobody ever added.

When nothing resolves it is simply not connected, so it appears in the add
dialog's **Cloud** list like anything else that is not connected, with the
endpoint host as its detail line. One rule, no special case.

Picking it opens a dialog with two ways in, because it has two: a **key**, which
writes `provider/tinyhumans/key`, and the **account link**, which writes the
company's TinyHumans identity. The dialog offers the key field and *links* to
Connections → Account for the other. The account credential already has a home
there, it has a different lifecycle — it is rotated and it moves every brokered
surface at once — and a second form for one credential is how two surfaces come
to disagree about whether a company has it.

### The one place the simple rule does not settle it

At the instance step the chain resolves, so on the plain "only what is not yet
connected" rule Managed would disappear from the list — and with it the only
route from *the server pays* to *we pay*. **It stays listed there**, deliberately.
The cost is that one entry can be in the list while a row for it is on the page;
the benefit is that a capability does not vanish. The explanation lives on the
Connected row ("Billed to whoever runs this server"), so the list stays uniform.

## The dialogs — as shipped

```
┌─ Add a provider ────────────────────────┐  ┌─ Add cloud provider ──────────┐
│ Pick a provider to connect. You can add │  │ The key is stored on this     │
│ more at any time.                       │  │ company and never shown again.│
│                                         │  │                               │
│ Cloud                                   │  │ Name                          │
│ [ Choose a cloud provider…           ▾] │  │ [ My Provider              ]  │
│ Hosted models. You supply an API key.   │  │ Slug: my-provider             │
│                                         │  │                               │
│ Local runtimes                          │  │ OpenAI URL                    │
│ [ Choose a local runtime…            ▾] │  │ [ https://api.openai.com/v1]  │
│ Models running on this machine. You     │  │                               │
│ supply the endpoint.                    │  │ API Key                       │
│                                         │  │ [ sk-...                   ]  │
│ CLI logins                              │  │                               │
│ [ Choose a CLI login…                ▾] │  │        [Cancel] [Add Provider]│
│ Not available on this host.             │  └───────────────────────────────┘
│ ─────────────────────────────────────── │
│ [ Add a custom provider ]               │
│ Your own OpenAI-compatible endpoint     │
└─────────────────────────────────────────┘
```

Dropdown items are two lines — the mark, the name, and a **monospace** detail —
because every one of those details is an address or a statement about where the
thing runs rather than prose.

Before a name is typed the slug line reads `Slug: None` with the inline error
*"Enter a provider name to generate a slug."*, and Add stays disabled.

**One thing deliberately not ported.** The reference's custom-provider subtitle
renders a duplicated interpolation — `auth-profiles.json auth-profiles.json.` —
which is a bug in the screenshot, not a detail to reproduce faithfully. Ours
says where the key goes, or nothing.

## Three categories, because they ask three different questions

Copy is verbatim from openhuman; the full list of options is in
[`catalogue.md`](catalogue.md).

```
┌─ Add a provider ────────────────────────────────────────────┐
│                                                             │
│  Cloud                                                      │
│  Hosted models. You supply an API key.                      │
│  [ Choose a cloud provider…                             ▾]  │
│                                                             │
│  Local runtimes                                             │
│  Models running on this machine. You supply the endpoint.   │
│  [ Choose a local runtime…                              ▾]  │
│                                                             │
│  CLI logins                                                 │
│  Reuses a login another command line tool already holds.    │
│  [ Choose a CLI login…                                  ▾]  │
│                                                             │
│  ────────────────────────────────────────────────────────   │
│                             [ Add Custom Provider ]         │
└─────────────────────────────────────────────────────────────┘
```

openhuman's reasoning, which is why this is three selects and not one list:

> The categories are not three slices of one decision, they are three different
> questions: a cloud provider wants an API key, a local runtime wants an endpoint
> on this machine, a CLI login wants nothing because another tool already holds
> the credential. One flat list makes the user infer that from the group heading
> alone; a select per category has a label and a line of helper text to say it
> outright.

Row detail lines: cloud shows the endpoint's **host**; local shows `Runs on this
machine`; CLI shows `Uses a login another CLI already holds`.

Each list shows **only what is not yet connected** — the page behind the modal
shows the rest, and offering to add something twice is how you get two rows for
one provider. The select's value stays pinned empty: choosing an item starts a
connect flow and leaves nothing selected, because the connection state lives in
the page rather than the control.

On a server-side host the **CLI logins** category has no options. Render it
saying so rather than hiding it — the shape is then right if a delegated
credential ever becomes available, and an empty labelled group is more honest
than a missing one.

## The flow

```
  pick ──▶ type ──▶ write key ──▶ flush record ──▶ PROBE ──┬─▶ ok ──▶ saved
                                                            │
                                                            └─▶ classify
                                                                   │
                    ┌──────────────────────────────────────────────┤
                    ▼                                              ▼
              reason == auth                              anything else
                    │                                              │
        roll back record AND key                      KEEP the key, keep the
        reject: "Could not reach X"                   record, show an amber
                                                      advisory on the row
```

Ordering matters and is not arbitrary:

1. **Validate locally** what can be validated locally — URL scheme and shape for
   an endpoint the operator typed. Reject before any write.
2. **Derive the slug** from the label for a custom provider, and reject a
   collision or a reserved word *before* writing anything.
3. **Write the credential first**, then flush the record. The probe needs the key
   resolvable by slug, so the credential has to land first.
4. **Probe.** `GET {base_url}/models` — cheap, read-only, and the same call the
   model picker needs anyway.
5. **Roll back both stores on a destructive failure**, and log a rollback failure
   rather than swallowing it. A silently failed key-clear orphans a secret.

## The probe is classified, not boolean

This is the part worth copying most carefully, because the naive version destroys
valid credentials.

```
                   probe error text
                          │
                          ▼
        ┌───────────────────────────────────────┐
        │ 407 / proxy / cloudflare / bad gateway│──▶ unknown   (NON-destructive)
        └───────────────────────────────────────┘      ▲
                          │ no                         │  checked FIRST, on purpose
                          ▼                            │
        ┌───────────────────────────────────────┐      │
        │ 401, or 403 WITH credential wording   │──▶ auth      (destructive)
        └───────────────────────────────────────┘
                          │ no
                          ▼
        ┌───────────────────────────────────────┐
        │ "model not found" / unknown model     │──▶ model     (non-destructive)
        └───────────────────────────────────────┘      ▲
                          │ no                         │  BEFORE endpoint: the
                          ▼                            │  endpoint branch matches
        ┌───────────────────────────────────────┐      │  a bare "not found"
        │ 404 / not found / DNS / refused       │──▶ endpoint  (non-destructive)
        └───────────────────────────────────────┘
                          │ no
                          ▼
        ┌───────────────────────────────────────┐
        │ timeout / deadline                    │──▶ timeout   (non-destructive)
        └───────────────────────────────────────┘
                          │ no
                          ▼
                      unknown                    ──▶ unknown   (non-destructive)
```

**Only `auth` deletes the key.** Everything else keeps it and shows an advisory,
because the key is plausibly fine and the connection is not.

Two branch-ordering rules that exist because of real bugs:

- **The proxy branch runs first.** Otherwise the word "authentication" inside
  `407 Proxy Authentication Required` matches the auth branch, and a corporate
  proxy deletes a valid key. A WAF's bare `403 Forbidden` has the same shape.
  This is why a 403 counts as `auth` only when it co-occurs with credential
  wording, and why the digit tests use word boundaries — so `401`/`403` do not
  match inside an id like `1403`.
- **`model` precedes `endpoint`.** The endpoint branch matches a bare "not
  found", which would otherwise claim every provider that phrases a missing model
  as "model not found" and send the operator off to check their base URL instead
  of their model id.

### Classification is separate from copy

Decide the class in one function; render the sentence in another. That keeps the
decision unit-testable without a translator, and keeps strings where they belong.

**The `unknown` branch must not interpolate the upstream string.** It can echo
request material — headers, key fragments — and it lands in a screenshot-able
banner. Put the raw text in a detail/console channel, not in the copy.

## Testing a draft, and the SSRF question

Today `POST {scope}/inference/test` probes the **saved** config. A list where you
add-then-test needs to probe a **draft** — an endpoint and key that are not
stored yet.

That route already exists for first-run: `POST /api/v1/setup/inference/test`
takes a key, uses it, and discards it, under a comment saying testing a credential
and committing to it are separate acts.

Generalising it to company scope creates an authenticated "send a request to an
arbitrary URL with an arbitrary key" primitive. **That is an SSRF-shaped
question and the plan answers it explicitly rather than inheriting it:**

- Require `AdminScopedCompany` — this is a company-deciding action.
- Restrict the scheme to `http`/`https`.
- Refuse link-local and cloud metadata addresses (`169.254.0.0/16` and friends)
  outright; a company's model endpoint is never there.
- Allow loopback only where the local-runtime category is offered at all, since
  that is exactly what Ollama needs — and make that an explicit allowance rather
  than a hole.
- Do not follow redirects to a host that fails the above.
- Cap body size and time; discard the body except for classification.

## What the operator sees on failure

| Class | Key | Row | Copy |
|---|---|---|---|
| `auth` | deleted | not created | "Could not reach *X*: the provider rejected the credential." |
| `endpoint` | kept | created, advisory | "Saved, but nothing answered at *host*." |
| `model` | kept | created, advisory | "Saved. The endpoint did not recognise that model id." |
| `quota` | kept | created, advisory | "Saved. The account is out of credit." |
| `timeout` | kept | created, advisory | "Saved, but *host* did not answer in time." |
| `unknown` | kept | created, advisory | "Saved, but the check did not complete." |

The advisory is amber and dismissible, keyed by slug. The save succeeded; only
reachability is in question. Colouring it as an error would be a lie about what
happened.

## Add without verifying

A provider that does not serve an OpenAI-shaped `{base}/models` listing is still
usable for inference. Blocking creation on the probe leaves those operators with
no way to reach the model field at all.

So offer "add anyway" — but gate it on a **typed probe-failure error**, never a
boolean. Only a probe failure unlocks it; a slug collision or a key-write failure
must not. And clear it on every retry, so an attempt that fails for an unrelated
reason does not still offer to skip verification.
