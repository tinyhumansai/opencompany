# Connecting: the key, the probe, and the failure handling

Adding a Composio key, and the failure classification that is the actual
substance of it. The rules here are ported from the inference rework's
`connect-flow.md`; the two places this surface deliberately differs are called
out where they occur.

## A modal after all, and why the first answer was wrong

This section argued the opposite until it was seen running. The original
argument, kept because half of it is still right:

> Inference opens a modal because a company picks one or two providers out of a
> couple of dozen, and listing them all inline spends the page on the ones
> nobody chose. Composio has **two routes and no catalogue**. There is nothing
> to pick from, so there is nothing for a modal to hold. The key field belongs
> on the row it configures. A dialog over one option is the same mistake as a
> select over one option — a button wearing a costume.

What that reasons about is **what a modal holds**, and it is correct about it:
there is no list here, and nothing to choose. What it never reasons about is
**where an inline field lands**, and that is what broke.

"On the row it configures" was not what inline produced. A row cannot hold a
password field, a hint, a Save, a Cancel and an advisory without becoming a
card, so the field rendered as a card appended after the rows — and after the
"takes effect next turn" line, and after two advisory slots. An operator
clicking `Add a token` on a row near the top of a scrolling page got a form
roughly a screenful below the fold, with nothing scrolling to it. The page did
not visibly move. It was reported as a dead button, which is the honest reading.

The fix could have been `scrollIntoView`. It is a modal instead, because the
position was only the symptom: an inline form's distance from its own button is
a function of how many advisories happen to sit between them, so it is correct
by luck on the render you tested and wrong on the next one.

And the action fits the shape. Pasting the credential every agent in the company
presents is not an edit alongside the rows — it is one decision taken to the
exclusion of the page behind it, which either lands or is refused before
anything else can be touched. That is what a modal is for, and it is a different
question from whether there is a catalogue to put in one.

What follows from it, and is load-bearing:

- **One exit.** Cancel, the X, Escape and the backdrop all run through
  `closeForm`, so no dismissal leaves a pasted secret in state behind a closed
  modal. A write in flight holds the dialog open.
- **A refusal renders inside the dialog**, next to the field, with `add anyway`
  where that applies — behind a modal overlay, a message on the page is a
  message nobody can read.
- **An advisory does not.** It means the key landed and only the check failed,
  so the dialog closes on it; the message is carried by a toast, because closing
  the dialog remounts the section that would otherwise hold it.
- **The managed → BYOK confirmation stays inside** that dialog rather than
  opening a second one, as a labelled group. It is about the key in the field
  above it, and a second overlay would hide the thing being decided.

## The sober layout

Two cards, no explanatory paragraphs. The page this replaces opened with prose
explaining what bring-your-own-key means, what Test costs and what Remove key
does — every one of them explaining a control that was visible while it was being
read.

A row is **a mark, a name, one sub-line and controls on the right**. The sub-line
is one fact chosen by what the row is; the table is in
[`resolution.md`](resolution.md).

Explanations survive the deletion pass only where they carry something no control
does. Two do:

- **The managed → BYOK confirmation.** Switching hides every provider connected
  through the managed route. That consequence is invisible in the control, it is
  not reversible by clicking back, and it is not obvious from "use your own
  account".
- **The amber advisory after a non-destructive probe failure.** The key was
  saved; only reachability is in question.

## Controls: never render one that cannot act

The rule that removed the toggle from the Managed inference row.

| Row | Control | Shown when |
|---|---|---|
| Managed | *(no toggle)* | never — managed cannot be switched off |
| Managed | `Use this` | BYOK is active **and** the managed chain resolves |
| Own key | `Use this` / `Add key` | managed is active |
| Own key | `Test` | a key is stored |
| Own key | `Replace key` | a key is stored |
| Own key | `Remove key` | **never** — see below |
| Managed | `Add a token` | managed is active, or the managed chain resolves to `none` |
| Managed | `Replace` / `Remove token` | a managed-route token is stored |

Two rows in that table are the sharp ones.

**`Remove key` on the own-account row is never offered**, and that is a shape of
the host rather than a decision anyone is free to reverse. The host derives the
route from whether a key exists, so `setComposioApiKey("")` writes the mode back
to `managed` as a side effect — the *same* call the managed row's `Use this`
makes. Rendering one action twice under two names is how an operator comes to
believe they are two. `composioRows` therefore sets `removeKey: false`
unconditionally on that row, and this table said the opposite until it was
caught in review.

**When `managedCredentialSource` is `none`, the managed row offers no `Use
this`** — switching to a route that resolves to nothing is an outage, not a
choice. It offers `Add a token` instead, which is the only order that works:
storing `composio/token` does not move the company off BYOK, so the credential
is provisioned first and the switch taken second, once the route resolves. The
sub-line says which payer failed to resolve while that is still true.

## The flow

```
  type key ──▶ PROBE ──┬──▶ ok ─────────────▶ store ──▶ Active
                       │
                       └──▶ classify
                              │
            ┌─────────────────┤
            ▼                 ▼
      class == auth      anything else
            │                 │
     store NOTHING      STORE the key, show an
     reject with copy   amber advisory on the row
```

### Probe the draft — this surface does not need rollback

**This is the deliberate deviation from the inference flow, and it is a
simplification.**

Inference writes the credential *first* and then probes, because its probe
resolves the key by slug and so needs it in the store to be reachable. That
forces a rollback path: on an auth failure it must delete both the record and the
key, and a silently failed key-clear orphans a secret.

Composio's probe takes the key **directly** — the endpoint is a constant and the
key is the whole credential. So a *draft* key can be validated before anything is
written. On an auth failure **nothing was ever stored**, so there is nothing to
roll back and no orphaned-secret failure mode to get wrong.

The inference doc's own "Testing a draft" section endorses exactly this; Composio
simply happens to be able to do it for every case rather than only first-run.

The clear path (`apiKey: ""`) is **never probed**. Clearing is always allowed.

### No SSRF guard, and why that is not an oversight

The inference probe generalises to "send a request to an arbitrary URL with an
arbitrary key", which is an SSRF-shaped primitive and needs scheme restrictions,
link-local and metadata refusals, and redirect rules.

Composio's probe has **no operator-supplied URL**. It dials the compile-time
constant `DIRECT_BASE_URL` (`https://backend.composio.dev`) and nothing else. The
route accepts a key, never an endpoint. There is no address for a caller to
steer, so there is no guard to write — and adding one would imply the endpoint
were variable, which is the misleading half of a defensive measure.

## The probe is classified, not boolean

The naive version destroys valid credentials. Branch order, and only the first
match wins:

```
                probe error text
                       │
                       ▼
   ┌────────────────────────────────────────┐
   │ 407 / proxy / cloudflare / bad gateway │──▶ unknown   (NON-destructive)
   └────────────────────────────────────────┘      ▲
                       │ no                        │ checked FIRST, on purpose
                       ▼                           │
   ┌────────────────────────────────────────┐      │
   │ 401, or 403 WITH credential wording    │──▶ auth      (DESTRUCTIVE)
   └────────────────────────────────────────┘
                       │ no
                       ▼
   ┌────────────────────────────────────────┐
   │ 429 / rate limit / out of credit       │──▶ quota     (non-destructive)
   └────────────────────────────────────────┘
                       │ no
                       ▼
   ┌────────────────────────────────────────┐
   │ 404 / not found / DNS / refused        │──▶ endpoint  (non-destructive)
   └────────────────────────────────────────┘
                       │ no
                       ▼
   ┌────────────────────────────────────────┐
   │ timeout / deadline                     │──▶ timeout   (non-destructive)
   └────────────────────────────────────────┘
                       │ no
                       ▼
                   unknown                   ──▶ unknown   (non-destructive)
```

**Only `auth` refuses to store.** Everything else stores the key and shows an
advisory, because the key is plausibly fine and the connection is not.

Three ordering rules that exist because of real bugs elsewhere in this org:

- **The proxy branch runs first.** Otherwise the word "authentication" inside
  `407 Proxy Authentication Required` matches the auth branch, and a corporate
  proxy causes a valid key to be rejected. A WAF's bare `403 Forbidden` has the
  same shape — which is why a `403` counts as `auth` only when it co-occurs with
  credential wording.
- **Digit tests use word boundaries**, so `401` and `403` do not match inside an
  id like `1403`.
- **There is no `model` class.** Inference needs one because a missing model id
  and a missing endpoint phrase themselves alike. Composio has no model concept,
  so the class would never fire and would only invite someone to route a real
  failure into it.

### Classification is separate from copy

`classify(err) -> Class` and `describe(class) -> &str` are two pure functions.
That keeps the decision unit-testable without a translator and keeps strings
where they belong.

**The `unknown` branch must not interpolate the upstream error string.** It can
echo request material — headers, key fragments — and it lands in a
screenshot-able banner. `describe` returns a fixed string per class; the raw text
goes to a tracing channel only.

## What the operator sees

| Class | Key | Row | Tone |
|---|---|---|---|
| `auth` | **not stored** | unchanged | destructive error: the credential was rejected |
| `quota` | stored | active, advisory | amber: saved, the account is out of credit |
| `endpoint` | stored | active, advisory | amber: saved, but nothing answered |
| `timeout` | stored | active, advisory | amber: saved, but it did not answer in time |
| `unknown` | stored | active, advisory | amber: saved, but the check did not complete |

The advisory is amber and dismissible. The save succeeded; only reachability is
in question. Colouring it as an error would be a lie about what happened.

## Add without verifying

A key may be valid while the probe cannot complete — a proxy, an outage, a
network this host cannot leave. Blocking on the probe would leave those operators
with no way to configure Composio at all.

So the console offers **add anyway**, which sends `skipVerify: true`. Two rules:

- It is offered **only after a probe failure**, never up front. A key nobody has
  tried does not need an escape hatch.
- It is **cleared on every retry**, so an attempt that fails for an unrelated
  reason — a store write error, a permissions refusal — does not still offer to
  skip verification.

The route honours `skipVerify` without re-deriving whether an escape was earned;
gating it on a prior failure is the console's job, because the route cannot see
the attempt that failed.

## Authority

`PUT …/composio/api-key` and the probe take `AdminScopedCompany`. Reads stay open
to any member.

That split is issue #403's and does not change here: a connection is the account
the company's *agents* act through, so it is company property in the same sense
the roster is. Knowing *that* Gmail is connected is what lets a member understand
why an agent can read mail; being able to change it is the part that needed an
owner.
