# Signals + Opportunity Studio

> A studio that never stops listening: it gathers market signals, turns them into ranked opportunities, and hands you a weekly brief — you decide what to fund.

## What it can do

- Continuously gather raw signals from the web, forums, and anything piped into
  its channels.
- Cluster the noise into real, recurring pains — not one-off complaints.
- Rank the resulting opportunities by evidence and size.
- Deep-dive the top few and compile a weekly brief you can act on.
- Keep everything it learns, so next week starts smarter than this one.

## Agent roster

| Agent | Responsibility |
| --- | --- |
| Signal Scout | Continuously gather raw market signals from web, forums, and inbound channels. |
| Opportunity Analyst | Cluster signals into pains and rank opportunities by evidence and size. |
| Research Agent | Deep-dive the top opportunities and compile the weekly brief. |

## The weekly loop

A single schedule drives the studio. Every Monday morning it scans the week's
signals, clusters them into pains, ranks the opportunities, and sends you a
ranked brief:

```toml
[[schedule]]
cron = "0 6 * * 1"
prompt = "Scan this week's signals, cluster them into pains, rank the top opportunities, and send the operator a ranked brief."
```

Signals arrive the same way any other work does: through the studio's channels
(operator and email) and the company's inbound webhook path,
`POST /hooks/{company}/{channel}`. Nothing new is bolted onto the runtime.

## Human in the loop

You keep **deciding which opportunities to fund and pursue**. The studio does
the listening, clustering, ranking, and writing; you make the call. Policy does
not create approval prompts automatically. Before funding an opportunity,
making a payment, filing, or publishing externally, the agent calls
`request_approval` with the exact decision and waits for your answer. The output
of this harness is **a ranked weekly opportunity brief**.

## Signals and the Opportunity Engine are a template, not kernel code

There is no "Signals subsystem" or "Opportunity Engine" inside OpenCompany.
This studio *is* the Opportunity Engine, expressed entirely over the existing
host:

- **Ingress** — channels and the inbound webhook path carry raw signals in.
- **Memory** — the runtime's memory and context stores hold the signal corpus
  across weeks.
- **Reasoning** — the brain does the clustering, ranking, and drafting.
- **Cadence** — the `[[schedule]]` entry runs the loop on its own.

Anyone can fork this manifest, point it at their market, and have their own
opportunity studio — no changes to the kernel required.

## Tool servers

A weekly brief is only as good as its sources: current documentation for what a signal claims, and the workspace the briefs accumulate in.

Declared in [`mcp.json`](mcp.json) and merged with anything the install
ships and anything an operator adds from the console. A server marked
*needs a token* is declared but off: write its credential from
Settings → Connections, then enable it there.

| Server | What it is for | Ships |
| --- | --- | --- |
| `context7` | Version-accurate API and library documentation, so answers match the release in use. | on |
| `notion` | The workspace this company's documents already live in. Needs a token. | off — needs a token |

## Run it

```sh
cargo run --bin opencompany -- serve --company companies/signals_opportunity_studio
```

Or validate the manifest without booting:

```sh
cargo run --bin opencompany -- check companies/signals_opportunity_studio
```
