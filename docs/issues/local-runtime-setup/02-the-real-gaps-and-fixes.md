# The real gaps, and the fix for each

Six, ranked by what they cost an operator. Gap 1 is a correctness bug; the rest
are the reasons it took a rebuild and a live session to find it.

---

## Gap 1 — the add flow claims a provider was saved when none was {#gap-1}

**Severity: high. This is the one to fix first.**

`probe_draft_inner`
(`crates/opencompany-core/src/server/ops/inference/providers.rs:2501`) builds
its failure message with `probe::describe` (line 2554):

```rust
let message = if failure.truncated {
    format!("The model list from {subject} could not be read.")
} else {
    probe::describe(failure.class, &subject)
};
```

`probe::describe` (`company/inference/probe.rs:332`) is written for a probe
that ran *after* a row was stored. Every arm says so:

```rust
ProbeClass::Endpoint => format!("Saved, but nothing answered at {provider}."),
ProbeClass::Model    => "Saved. The endpoint did not recognise that model id.".to_string(),
ProbeClass::Quota    => "Saved. The account is out of credit.".to_string(),
ProbeClass::Timeout  => format!("Saved, but {provider} did not answer in time."),
ProbeClass::Unknown  => "Saved, but the check did not complete.".to_string(),
```

The draft probe stores nothing. It runs before any write, on a draft the
operator has not committed to. So the console tells them their provider was
saved, on a screen where the Connect button has not been pressed.

**The fix already exists in the file.** `probe::describe_refusal`
(`probe.rs:356`) is the same table written for the not-connected case, and its
`Endpoint` arm carries the advice this flow wants:

```rust
ProbeClass::Endpoint => format!(
    "Nothing answered at {subject}, so it was not connected. Start it and try again."
),
```

Call that instead. One line.

**Two caveats before doing it.**

*It is not a pure swap.* `describe_refusal`'s wording ("so it was not
connected") is right for a rollback after an attempted add, which is where it is
used today (`providers.rs:735`). On the draft probe nothing was attempted
either, so the sentence is still true, but read it once in context before
shipping — "it was not connected" and "you have not connected it yet" are
different claims, and the second is the accurate one here. A third arm on the
same table, rather than reuse, is a legitimate outcome of that reading.

*Name the authority, not the host.* Both functions interpolate a `subject`
built by `catalogue::endpoint_host`, which drops the port. Every local runtime
lives on `localhost`; the port is the entire distinguishing information, and a
message reading "nothing answered at localhost" is unactionable when three
runtimes share that host. Use host **and** port here.

### The consequence worth noticing

Because this string is interpolated into `modelAskFromProbe`'s own sentence
(`frontend/src/inference/connect.ts:560`), the operator actually reads:

> Could not read this provider's models: Saved, but nothing answered at
> localhost. Type a model id.

Two verdicts in one line, one of them false.

---

## Gap 2 — no way to test an endpoint before committing to it {#gap-2}

**Severity: high.**

The endpoint step (`frontend/src/inference/ProviderConnectDialog.tsx:508-555`)
offers Cancel and Continue. Continue fires the draft probe and advances. There
is no way to ask "does this address work" without leaving the screen that holds
the address.

The product already disagrees with itself here: `TestControl` in
`ProviderList.tsx` gives every *saved* row a Test button.

**The fix.** Add `Test connection` to the endpoint step, calling the probe
`Continue` already calls — `actions.probeDraftEndpoint`, same arguments — and
rendering its answer under the field instead of advancing:

- ok → `Reachable — N models published.`
- not ok → the gap-1 sentence, on the field that can be fixed.

Nothing new is built. The probe, its classification, its cancellation guard
(the `attempt` counter in `ProvidersTab.tsx`) and its DTO all exist. This is an
affordance over machinery that already runs.

**Do not make Test mandatory.** A runtime that is momentarily down should not
block an operator who knows their address is right; `Continue` already handles
the failure path and `Add anyway` already exists for the committed case.

---

## Gap 3 — a failed probe dead-ends into free text {#gap-3}

**Severity: medium. This is what made the report read as "no dropdown".**

On failure the model step opens on free text with a sentence and no way
forward except typing an id or pressing Back. Nothing says the endpoint is the
thing to fix, and nothing offers to try again.

The precedent for doing better is in the same file: an `auth` failure already
stops on the endpoint step rather than advancing
(`ProvidersTab.tsx:303`), because a rejected credential must be fixed on the
field that holds it. **An `endpoint` failure has exactly the same property** —
the address is wrong or the runtime is down, and neither is repaired by typing
a model id.

**The fix, in preference order.**

1. For a local runtime with an `endpoint`-class failure, hold the endpoint step
   the way `auth` is held, rather than advancing into a model step that cannot
   help. This is the smallest change that makes the flow honest.
2. If advancing is kept, carry a retry affordance into the model step rather
   than leaving Back as the only exit.

Option 1 subsumes gap 2 for the failure case, which is an argument for doing
both in one pass.

---

## Gap 4 — two of three runtimes ship no default endpoint {#gap-4}

**Severity: medium.**

`LOCAL_RUNTIMES` (`frontend/src/inference/catalogue.ts:283`, mirrored at
`crates/opencompany-core/src/company/inference/catalogue.rs:420`) gives a
`defaultEndpoint` to `ollama` and to nothing else. LM Studio and OMLX open on
an empty field whose placeholder reads `https://api.openai.com/v1` — a cloud
URL, on a local-runtime screen.

**The fix.** Give LM Studio `http://localhost:1234`, its conventional
OpenAI-compatible server address.

**OMLX is deliberately left open.** Its port was not confirmed during this
work, and a wrong default is worse than none: it turns "empty field, you must
know the port" into "prefilled field that silently fails", which is strictly
harder to diagnose. Somebody who has run OMLX should fill this in, or it stays
blank.

**Both tables move together.** The console catalogue and the Rust catalogue are
mirrored and a parity test asserts they agree; changing one alone is a red
build, not a silent drift.

While here: the placeholder on the endpoint input should not be an OpenAI URL
when the dialog is a local runtime.

---

## Gap 5 — "this machine" means the host, and the UI implies otherwise {#gap-5}

**Severity: medium — and it is the whole story on a hosted tenant.**

`COPY.helperLocal` (`catalogue.ts:385`) reads:

> Models running on this machine. You supply the endpoint.

The address is resolved by the machine hosting the company. On the desktop app
that is the operator's laptop and the sentence is true. On a hosted tenant it is
the server, which is not running their Ollama and never will be — so no
endpoint they can type will work, and the copy actively encourages them to keep
trying ports.

`LOCAL_RUNTIMES`' own doc comment states this. The UI contradicts it.

**The fix, minimum:** say the host.

> Models running on the machine that hosts this company — not your laptop. You
> supply the endpoint.

**The fix, better:** on a deployment where the host is not the operator's
machine, say so where it matters. The host already knows it is a hosted tenant
(`OPENCOMPANY_DEPLOYMENT` / `OPENCOMPANY_TENANT_ID`, per
`docs/spec/runtime/analytics.md` and the storage spec). Whether the local group
should be disabled outright there, or merely labelled, is a product call this
brief does not make — but it must not stay silent.

---

## Gap 6 — a model id is mandatory with no help when the lookup fails {#gap-6}

**Severity: low, and partly by design.**

Since the keys rework (#2306, decision D-model) a model is required everywhere
`ModelField` is used — there is no blank passthrough. Correct. But on a failed
probe the operator must supply an exact id from memory, with no list and no
validation, and the only feedback is at Connect time.

No fix is proposed beyond gaps 1–3: if the endpoint gets tested and repaired on
the step that owns it, this state is reached far less often. Recorded so that a
future "why is this a free text box" question finds the answer rather than
re-opening the same investigation.

---

## What is explicitly *not* proposed

- **A new model picker for local runtimes.** One exists. See `01`.
- **Changing the normalization.** Both halves are correct. See `01`.
- **A background reachability poller.** `known-defects.md` argues against one
  for provider health and the argument holds here: it costs a request per
  provider per interval to learn what the next real action learns for free.
- **Any change to the SSRF policy.** The loopback allowance is load-bearing.
