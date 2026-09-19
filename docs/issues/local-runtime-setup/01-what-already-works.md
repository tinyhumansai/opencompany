# What already works

Read this before changing anything. It is the part of the system the reported
symptom made look broken, and it is not.

## How to reproduce the verification

Ten minutes, no stubs, no mocking. This is the setup every claim in this brief
was checked against.

```bash
brew install ollama
ollama serve &                     # 11434
ollama pull qwen2.5:0.5b           # a chat model
ollama pull all-minilm             # an embedding model, so the list has two shapes

cargo build --features openhuman,mcp,composio --bin opencompany
OPENCOMPANY_CONSOLE_DIR="$PWD/frontend/dist" OPENCOMPANY_DATA_DIR=<scratch> \
  ./target/debug/opencompany serve --bind 127.0.0.1:8080 --company companies/e2e_harness
```

Sign in with the dev magic link (loopback bind and no mail config make
`POST …/auth/request` return `dev_code` in its JSON), then drive
**Connections → LLM → Add a provider → Local runtimes → Ollama**.

Pull two models, not one. A single-entry list proves the combobox renders; it
does not prove the ranking path, and `all-minilm` is a non-chat entry, which is
what `probe_model_candidates` exists to sort around.

## The probe reaches a local runtime, and returns its catalogue

Four calls through the running host. The first is what the console actually
sends; the rest are the failure shapes an operator hits.

| Case | `POST …/inference/probe` answers |
|---|---|
| `http://localhost:11434/v1` — what the UI sends | `{"ok":true,"modelCount":2,"models":["all-minilm:latest","qwen2.5:0.5b"]}` |
| `http://localhost:11435/v1` — wrong port | `{"ok":false,"class":"endpoint","message":"Saved, but nothing answered at localhost."}` |
| `http://localhost:1234/v1` — LM Studio's port, nothing listening | *identical to above* |
| `http://localhost:11434` — bare origin | *identical to above* |

The success row is the one that closes the original question. The three
identical failure rows are [gap 1](02-the-real-gaps-and-fixes.md#gap-1).

## The normalization chain is correct on both sides

The endpoint an operator sees is a bare origin. The endpoint a catalogue read
needs ends in `/v1`, because `probe_models` asks `{base}/models`
(`crates/opencompany-core/src/company/inference/probe.rs:867`).

Both halves already close that gap, independently:

- **Console, before the request leaves the browser.** `normalizeEndpoint`
  (`frontend/src/inference/connect.ts:368`) appends `/v1` when the URL carries
  no path. `probeEndpoint` (`connect.ts:228`) routes a local runtime through it.
- **Host, before the row is stored.** `normalize_local_endpoint`
  (`crates/opencompany-core/src/company/inference/catalogue.rs:900`) does the
  same, and the add path calls it (`providers.rs:1084`, `:1138`).

So the catalogue default `http://localhost:11434`
(`catalogue.ts:283`, mirrored `catalogue.rs:420`) is probed as
`http://localhost:11434/v1/models`, which Ollama serves. **This is the
correction to the first wrong theory in the README** — do not "fix" it.

## The model step already renders the same combobox cloud providers get

There is one model field. `ModelField.tsx` decides between a catalogue select
and free text in `showsCatalogSelect` (`ModelField.tsx:42`), on four inputs,
none of which is "is this provider local":

- the operator asked for text (their choice wins),
- `freeTextOnly` — an Azure deployment name its own `/models` never publishes,
- an **empty list** — no catalogue, or a read that failed,
- a value the list does not contain.

A local runtime hits the third arm when its probe fails, and only then. With a
reachable runtime it takes the select, identically to OpenRouter: a filterable
combobox (`ModelCombobox`), `Choose a model` on an unset value
(`CHOOSE_MODEL_LABEL`, `ModelField.tsx:22`), the `Enter a model id instead`
link, and `cappedNote` when the list is long enough to be truncated.

Observed live: the model step opened on `Choose a model` with both pulled
models available, plus the escape-hatch link.

**Consequence for the fix:** nothing in the model step needs building. Every
change in this brief is upstream of it — about making the probe succeed more
often, and about telling the truth when it does not.

## The probe already classifies failures

`ProbeClass` distinguishes `Auth`, `Endpoint`, `Model`, `Quota`, `Timeout`,
`Unknown`, and the draft route already carries the class to the console as
`class` in `ProbeResultDto`. The console already acts on it: an `auth` failure
stops on the endpoint step rather than advancing
(`frontend/src/inference/ProvidersTab.tsx:303-307`), on the grounds that a
rejected key must be fixed on the field that holds it.

That precedent is the whole argument for [gap 3](02-the-real-gaps-and-fixes.md#gap-3):
an `endpoint` failure is equally fixable on the field that holds it, and is
equally not worth advancing past.

## A Test control already exists — on the wrong side of the save

`ProviderList.tsx` renders `TestControl` per connected row, titled *"Test
{label}. Sends one real request; your provider may charge for it."* The product
already believes an operator should be able to test a provider; it offers it
only once the provider is saved.

The endpoint step, meanwhile, has Cancel and Continue
(`ProviderConnectDialog.tsx:508-555`). That asymmetry is
[gap 2](02-the-real-gaps-and-fixes.md#gap-2), and it is why the fix there is
"expose the probe that already runs", not "build a checker".

## Loopback is a deliberate allowance, not an oversight

`probe::default_policy` sets `allow_loopback: !LOCAL_RUNTIMES.is_empty()`
(`probe.rs:698`). A deployment that offers local runtimes must accept loopback
probe targets, or the category cannot work at all; every other private range
stays refused (`probe.rs:606-620`).

Worth knowing before touching the SSRF guard: the allowance is load-bearing for
this whole feature, and the catalogue comment above `LOCAL_RUNTIMES` says so.

## What "local" means, and why it is not a bug here

`LOCAL_RUNTIMES`' own doc comment is explicit: on a server-side product these
addresses mean **the host's** localhost, not the operator's laptop; they
coincide only because the desktop app is both. The code is right. The UI copy
is not — see [gap 5](02-the-real-gaps-and-fixes.md#gap-5).
