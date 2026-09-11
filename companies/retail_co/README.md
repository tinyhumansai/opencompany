# retail-co

The [tau2-bench](https://github.com/sierra-research/tau2-bench) **retail**
domain, run as a company of three desks and five seats.

| desk | seats | remedy each seat holds | deliberates |
|---|---|---|---|
| `triage` | `triage` | none — nine read tools, zero mutating | no (one seat) |
| `order_ops` | `cancellations`, `amendments` | cancel the whole order / amend it in place | yes |
| `returns` | `exchanges`, `refunds` | swap for a variant / take it back | yes |

## Why it is shaped like this

**Scope is enforced below the model.** Each seat is granted exactly one MCP
server, and each server registers only the tools its role is scoped to. A seat
reaching outside its role does not violate a policy it was asked to respect —
it calls a tool that was never registered, and fails at the protocol layer.
`triage` cannot cancel an order however the conversation goes.

**The write desks are pairs, so the room has something to argue about.** A
desk of one cannot deliberate (`deliberates()` requires two). A delivered-order
problem can be answered with an exchange or with a refund, and those are
different seats holding different tools; a pending-order problem by cancelling
or by amending. Neither seat can reach the other's tool, so the remedy has to
be argued for rather than quietly done both ways. `quorum = 2` on a two-seat
desk means the remedy that carries is unanimous — the right bar for a write
nobody can reverse.

**It asks what tau2 cannot.** tau2's orchestrator wires exactly one agent to
one user simulator, with no agent-to-agent path, so it scores whether an agent
called the right tool — not whether an *organisation* routed the work to the
seat that owns it. Here a task only completes if the case reaches the right
desk and that desk settles which remedy applies.

## The servers this bundle declares

Five, one per seat, each `enabled: false` in `mcp.json` until a runtime
registration points it at a reachable endpoint:

| server | seat | what it can change |
|---|---|---|
| `tau2-retail-triage` | triage | nothing — nine read tools, zero mutating |
| `tau2-retail-exchanges` | exchanges | `exchange_delivered_order_items` |
| `tau2-retail-refunds` | refunds | `return_delivered_order_items` |
| `tau2-retail-cancellations` | cancellations | `cancel_pending_order` |
| `tau2-retail-amendments` | amendments | `modify_pending_order_items` / `_address` / `_payment` |

Each is granted to exactly one seat, so the write a seat can perform is the
whole of what it may do to the business.

## The servers are not in this repo

They live in `opencompany-tau2`, which vendors tau2-bench (~850 MB, mostly
benchmark data) and needs its own Python venv. This bundle therefore ships five
**disabled** `mcp.json` entries pointing at placeholder `https` hosts, because a
bundle here must not point an agent at a host nobody has provisioned — and
because runtime is the only layer that accepts an `http://` endpoint.

All five servers share ONE state file under an exclusive `flock`, so a
cancellation is visible to `triage` on its next read.

## Running it

Start the five role servers from the `opencompany-tau2` checkout:

```bash
uv run tau2-mcp --roles roles/retail.yaml --role triage        --http 8801 &
uv run tau2-mcp --roles roles/retail.yaml --role exchanges     --http 8802 &
uv run tau2-mcp --roles roles/retail.yaml --role refunds       --http 8803 &
uv run tau2-mcp --roles roles/retail.yaml --role cancellations --http 8804 &
uv run tau2-mcp --roles roles/retail.yaml --role amendments    --http 8805 &
```

### Credentials

The bundle carries the *routing* — provider, base URL, and every tier mapped to
`deepseek/deepseek-v4-flash` — but never the key. Set that per company, from the
console's Inference card or over the API:

```bash
curl -X PUT localhost:8099/api/v1/companies/retail-co/inference \
  -H 'content-type: application/json' \
  -d "{\"provider\":\"openrouter\",\"base_url\":\"https://openrouter.ai/api/v1\",\"key\":\"$OPENROUTER_API_KEY\",
       \"models\":{\"chat-v1\":\"deepseek/deepseek-v4-flash\",\"reasoning-v1\":\"deepseek/deepseek-v4-flash\",
       \"agentic-v1\":\"deepseek/deepseek-v4-flash\",\"vision-v1\":\"deepseek/deepseek-v4-flash\"}}"
```

**Send the `models` table with the key.** `PUT …/inference` stores the whole
config, and an omitted `models` becomes an empty map that then *shadows* this
bundle's `[inference.models]` — the next turn asks the provider for a default
model nobody chose. Observed: a `PUT` carrying only the key made the probe
request `anthropic/claude-sonnet-5`, which the account's allowed-providers
refused. `--check` catches this.

A company declaring `[inference]` consults its own `inference/key` secret, so
`OPENCOMPANY_INFERENCE_KEY` does **not** stand in for it — the first turn fails
with a 401 from the platform endpoint rather than from OpenRouter. Hosting
several tau2 companies on one `serve` means one `PUT` each.

Then, against a running host:

```bash
cargo run --features openhuman,hivemind,mcp --bin opencompany -- \
  serve --company companies/retail_co --home /tmp/retail
python3 scripts/tau2-sim.py --domain retail --task 0
```

Verify the whole rig before spending a model call — role servers reachable with
the exact tool scope each seat should have, desks staffed as intended, MCP
registered and reachable *through the host*, the credential probing clean, and
the tau2 state present:

```bash
python3 scripts/tau2-sim.py --domain retail --check
```

Exit status is the number of failed checks. Then `scripts/tau2-sim.py` repoints the five entries at loopback, replays the
task's opening message into `triage`, and grades the shared retail database
against tau2's own `evaluation_criteria`. Exit status is the number of tasks
whose end state did not match.

## Handing work on

Two mechanisms, and they are not interchangeable:

- **`@desk` in a reply** posts the case on that desk's channel, where its seats
  deliberate and send back what the room settled on. This is the hand-off to
  reach for when the choice between remedies is the question.
- **`delegate_to_teammate`** takes one turn from one named person, no room.

`delegate_to_desk` resolves to whoever leads the desk and takes one turn from
them, which skips the deliberation these paired desks exist for — the seats are
told not to use it.
