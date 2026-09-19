# Local runtime setup: test before you save, and stop claiming "Saved"

Implementation brief for issue #2403. Read this file first, then the other
three in order. Written so a fresh engineer or Claude session with none of the
conversation this came from can pick it up and execute correctly.

Every claim here was checked against a running system, not inferred. The
verification setup is in [`01-what-already-works.md`](01-what-already-works.md);
it is worth reproducing before changing anything, because it takes ten minutes
and it is what turned three of this design's earlier assumptions into
corrections.

## The headline finding: the dropdown is not missing

The originating report was "adding Ollama gives you a text box for the model,
not a dropdown like OpenRouter gets — make it robust like the cloud providers."

That framing is wrong, and acting on it would have built a second model picker
next to a working one. **Local runtimes already use the same model combobox
every cloud provider uses.** With a reachable runtime the add flow opens the
model step on a populated `Choose a model` list, complete with the filter box
and the `Enter a model id instead` escape hatch. Verified on screen against a
real Ollama 0.34.2 serving two models.

The text box the reporter saw is the **fallback**. `modelAskFromProbe`
(`frontend/src/inference/connect.ts:560`) turns a failed probe into an empty
catalogue plus a reason, and `showsCatalogSelect`
(`frontend/src/inference/ModelField.tsx:42`) then renders free text. That
degradation is deliberate and correct — an empty select would assert "this
provider publishes no models", which nobody established.

So the defect is not a missing control. **It is that the lookup fails quietly,
the failure is unactionable, and one of the sentences it prints is false.**

## The one that is a real bug, not a UX gap

Every failed draft probe answers:

```
Saved, but nothing answered at localhost.
```

Nothing was saved. `probe_draft_inner`
(`crates/opencompany-core/src/server/ops/inference/providers.rs:2501`) runs
**before** any write, and builds its message with `probe::describe`
(line 2554) — a function written for a probe that ran *after* a row was
stored (`crates/opencompany-core/src/company/inference/probe.rs:332`).

`probe::describe_refusal` sits nineteen lines below it
(`probe.rs:356`), is written for exactly this case, and already carries the
sentence this flow needs: *"Nothing answered at {subject}, so it was not
connected. Start it and try again."*

The add flow calls the wrong one. That is the whole bug, and it is the highest
value item in this brief: small, provably wrong, and it misinforms an operator
about whether their configuration landed.

## Corrections to this design's own history

Recorded because each cost time and each would be re-derived by the next
person reading only the symptom.

**1. "The probe asks the wrong URL."** The first theory was that the console
probes `{base}/models` while Ollama serves `/v1/models`, making the catalogue
default (`http://localhost:11434`, no `/v1`) unprobeable. **Wrong.**
`normalizeEndpoint` (`connect.ts:368`) appends `/v1` to a bare origin before
the request leaves the browser, and the save path normalizes identically via
`normalize_local_endpoint` (`catalogue.rs:900`). The probe asks the right
address — proven by the success case in `01`.

**2. "Local providers need their own model picker."** They have one. See above.

**3. "The failure is silent."** It is not silent, it is *misleading*, which is
worse. The dialog prints a sentence; the sentence is wrong about the most
important fact in it.

## What the fix actually is

Four changes, none large, in this order:

1. Call `describe_refusal` from the draft probe, and name the **authority**
   (host *and* port) rather than the bare host.
2. Add `Test connection` to the endpoint step, running the draft probe that
   `Continue` already fires — without advancing the dialog.
3. Give LM Studio a default endpoint, and correct `helperLocal` to say the
   host machine rather than "this machine".
4. Make the model step's failure offer a way back, instead of dead-ending
   into free text.

Detail and reasoning: [`02-the-real-gaps-and-fixes.md`](02-the-real-gaps-and-fixes.md).
Sequencing, edge cases and blast radius:
[`03-rollout-and-edge-cases.md`](03-rollout-and-edge-cases.md).

## The flow, as it is today

The hidden step is the third one. `Continue` is not navigation — it performs a
network round trip, and its only tell is the button label changing to
"Reading models…" (`ProviderConnectDialog.tsx:545`).

```
┌─ Add a provider ────────────────────────────────┐
│  Cloud            [ Choose a cloud provider… ▾ ]│
│  Local runtimes   [ Choose a local runtime…  ▾ ]│ ── Ollama / LM Studio / OMLX
│  CLI logins       [ Not available on this host ]│
│  [ Add a custom provider ]                      │
└─────────────────────────────────────────────────┘
                     │  pick Ollama
                     ▼
┌─ Connect Ollama ────────────────────────────────┐
│  Endpoint  [ http://localhost:11434           ] │ ◀── LM Studio and OMLX
│                                                 │     open this field BLANK
│                        [ Cancel ]  [ Continue ] │ ◀── no way to test
└─────────────────────────────────────────────────┘
                     │  Continue ── fires POST …/inference/probe
                     │              (label → "Reading models…")
        ┌────────────┴────────────┐
        │                         │
   probe ok                  probe failed
        │                         │
        ▼                         ▼
┌─ choose a model ──────┐  ┌─ choose a model ─────────────────────┐
│ Model                 │  │ Model                                │
│ [ Choose a model   ▾ ]│  │ [ Type a model id                  ] │
│   ├ all-minilm:latest │  │ "Could not read this provider's      │
│   └ qwen2.5:0.5b      │  │  models: Saved, but nothing answered  │
│ Enter a model id      │  │  at localhost. Type a model id."      │
│ instead               │  │        ▲                              │
│                       │  │        └── nothing was saved          │
│  [ Back ] [ Connect ] │  │  [ Back ] [ Connect ]                 │
└───────────────────────┘  └──────────────────────────────────────┘
   ← this already works       ← the reported symptom lives here
```

## The flow, as proposed

Only the middle panel changes shape; the right-hand branch changes its words
and gains a way out.

```
┌─ Connect Ollama ────────────────────────────────┐
│  Endpoint  [ http://localhost:11434           ] │
│  Reachable — 4 models published.                │ ◀── answer, in place
│                                                 │
│      [ Cancel ]  [ Test connection ] [ Continue ]│
└─────────────────────────────────────────────────┘

  on failure, same step, no navigation:
│  Nothing answered at localhost:11434, so it was │
│  not connected. Start it and try again.         │
```

## File map

| File | What it holds |
|---|---|
| [`01-what-already-works.md`](01-what-already-works.md) | The cited chain, and the live verification that settled it |
| [`02-the-real-gaps-and-fixes.md`](02-the-real-gaps-and-fixes.md) | Six gaps, ranked, each with its fix |
| [`03-rollout-and-edge-cases.md`](03-rollout-and-edge-cases.md) | Order, edge cases, blast radius, what to test |

## Scope

This brief is documentation only. The implementation is follow-up work and
#2403 stays open to track it.
