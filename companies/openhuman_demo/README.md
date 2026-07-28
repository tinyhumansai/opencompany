# OpenHuman Demo Co

> A three-agent company whose cognition runs on the **embedded OpenHuman
> runtime** (issue #9). It is the smallest end-to-end demonstration that an
> OpenCompany agent sits on the OpenHuman harness: one turn of chat goes
> operator → cycle → `HarnessBrain` → a live OpenHuman agent → reply.

## What it can do

- Answer questions about the company in the voice of whichever agent you address.
- Sketch a technical plan (via the Engineer).
- Turn rough notes into a short written draft (via the Writer).

## Agent roster

| Agent | Responsibility |
| --- | --- |
| Chief Executive | Sets direction, answers about the company, delegates the work. |
| Engineer | Explains how things are built and proposes technical plans. |
| Writer | Turns rough notes into short, clear written drafts. |

## Running it on the OpenHuman brain

The harness brain activates when an inference credential is present in the
environment; otherwise the company boots on the offline echo brain.

```bash
TINYHUMANS_API_KEY=<jwt> \
OPENCOMPANY_INFERENCE_URL=https://api.tinyhumans.ai/openai/v1 \
cargo run --features openhuman --bin opencompany -- \
  serve --company companies/openhuman_demo
```

Then chat with it:

```bash
curl -s localhost:8080/api/v1/company/chat \
  -H 'content-type: application/json' \
  -d '{"text": "Who are you, and what does this company do?"}'
```

For a credential-only smoke without the HTTP server, the
`live_company_turn` example runs a single turn and prints the metered usage:

```bash
TINYHUMANS_API_KEY=<jwt> \
cargo run --features openhuman --example live_company_turn -- "Introduce yourself."
```

## Human in the loop

You keep **steering the company and approving anything costly**; the agents run
everything else. The harness's output is **answers, plans, and short written
drafts**.
