# Agentic Accounting Firm

> An accounting firm of agents that keeps books, prepares taxes and payroll, forecasts, and readies audits — a human signs off on filings.

## What it can do

- Keep the books and reconcile accounts.
- Prepare taxes.
- Run payroll.
- Build financial forecasts.
- Prepare for audits.

## Agent roster

| Agent | Responsibility |
| --- | --- |
| Bookkeeper | Record transactions and reconcile accounts. |
| Tax Preparer | Prepare tax filings. |
| Payroll Agent | Run payroll and related filings. |
| Forecaster | Build financial forecasts and budgets. |
| Audit Preparer | Assemble documentation for audits. |

## Human in the loop

Humans keep **sign-off on filings**; the agents run everything else. The output of this harness is **books, taxes, payroll, and forecasts**.

## Run it

```sh
cargo run --bin opencompany -- serve --company companies/agentic_accounting_firm
```
