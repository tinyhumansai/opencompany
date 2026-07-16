# 05 — WS5: Usage & Finances (Metering)

## Scope

Turn the raw spend/usage data the runtime already models (`TokenUsage`,
`LedgerEntry`, `[budget]`, tiny.place economy payments) plus the new
`UsageSample` stream (WS4 cost hook) into the two console read surfaces:

- **Usage** — token burn over time, tokens by teammate, calls by provider,
  totals, over 7/30/90 days.
- **Finances** — balance, budget vs spend, revenue, spend by category,
  transactions.

No new port beyond WS3's `UsageMeter`; finances resolve entirely from
existing data.

## Design

### Aggregation module — `src/metering/`

```rust
pub fn bucket_usage(samples: &[UsageSample], range: UsageRange, now_millis: u64) -> Usage;
// - series: one UsagePoint per day in range (zero-filled), input/output tokens
// - by_agent: sum tokens per sample.agent (teammate display names resolved
//   from the roster; prosumer language)
// - by_provider: count SampleKind::OauthCall per provider
// - totals: token sums, cost_usd sum, oauth call count, connected count

pub fn finances_from(ledger: &[LedgerEntry], budget: &Budget,
                     economy_balance: Option<f64>, now_millis: u64) -> Finances;
// - spent_usd: current-month outgoing entries; budget_usd from [budget].monthly_usd
// - revenue_usd: incoming entries (payment.received / a2a sale receipts)
// - balance_usd: economy wallet when the tinyplace feature is on, else
//   revenue - spend (bookkeeping balance)
// - by_category: LedgerEntry.kind prefix mapping (inference.* -> "Inference",
//   tools.* -> "Tools", payment.* -> "Payments", …) with prosumer labels
// - transactions: ledger entries newest-first, paged
```

Pure functions over port data — trivially unit-testable, no I/O.

### Data flow

```
openhuman TurnCost ──(WS4 cost hook)──► LedgerEntry("inference.spend") ─► Finances
                                    └─► UsageMeter::record(UsageSample) ─► Usage
tiny.place payments ──(existing economy adapter journals)──► ledger ─────► Finances
OAuth tool calls ──(WS4 hook, SampleKind::OauthCall)──► UsageMeter ──────► Usage.byProvider
```

The GraphQL resolvers (WS2d, `graphql/usage.rs` + `graphql/finances.rs`) call
`UsageMeter::query(company, since)` / `CompanyStore::load(...).ledger` and
feed these functions.

### Retention

`UsageMeter` backends evict samples older than **90 days** (the console's max
range) — fs compacts `usage.jsonl` on write past a threshold; sqlite/mongo
delete by `at_millis`. (README open question 4; this is the recommended
default.)

### Spec alignment

- The ledger stays the single financial source of truth, as
  [`docs/spec/company-as-agent/commerce.md`](../spec/company-as-agent/commerce.md)
  assumes; metering never writes the ledger, only reads it.
- Budget exhaustion behavior is unchanged (pause + `budget.exhausted`
  webhook); Finances merely reports the same numbers.
- Category and teammate labels follow the glossary — no "cycle", no "tier".

## Subtasks (commit-sized)

1. `feat(metering): usage bucketing + finances projection (pure functions)`
2. `feat(graphql): usage + finances resolvers` (= WS2 subtask 6)
3. `feat(store): usage retention/eviction in fs/sqlite/mongodb`

## Dependencies

WS3 (`UsageMeter` port exists), WS4 (cost hook produces real samples —
resolvers can land earlier against seeded data). Feeds WS2d and WS7's Usage +
Finances views.

## Tests & exit criteria

Unit: bucketing across ranges (zero-fill, day boundaries), category mapping,
budget math, empty inputs. Feature: seed samples + ledger entries via ports,
assert the GraphQL aggregates. E2E: one chat round-trip produces a visible
`inference.spend` transaction and a usage point. Exit per
[09-verification.md](09-verification.md) WS5 row.
