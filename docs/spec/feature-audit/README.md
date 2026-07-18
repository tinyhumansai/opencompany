# Feature Audit

This directory records high-level product capabilities that OpenCompany may
need before it can reliably serve both a one-person business and a hosted
fleet. These are audit drafts, not commitments. They translate the current
product thesis into implementation-sized feature families that can be enriched
with detailed designs, milestones, and GitHub issues later.

The audit is grounded in the current repository:

- company definitions are version-controlled data under `companies/`;
- the Rust host owns lifecycle, storage, approvals, metering, and HTTP APIs;
- OpenHuman supplies the embedded teammate harness;
- Medulla is the preferred orchestration brain;
- the console is the Operator's primary control surface;
- tiny.place is the optional identity, discovery, A2A, and payment adapter.

## Feature families

| Spec | Outcome | Current foundation |
| --- | --- | --- |
| [Guided company blueprints](01-guided-company-blueprints.md) | An Operator can describe a business and review a validated company before launch | Templates, manifest validation, onboarding lifecycle, Architect design |
| [Active runtime teammates](02-active-runtime-teammates.md) | Every accepted teammate is a real, policy-bound worker | Manifest agents, roster overlays, embedded OpenHuman harness |
| [Executable workflows](03-executable-workflows.md) | Workflow graphs can be edited, run, paused, resumed, and inspected | Workflow TOML parser and read-only console canvas |
| [Live operations](04-live-operations.md) | The Operator sees work, approvals, failures, and replies as they happen | Event log subscriptions and request/response APIs |
| [Governance and permissions](05-governance-and-permissions.md) | Autonomy is bounded by understandable, testable policy | Approval gate, security tiers, secret store, audit journal |
| [Reliable and measurable execution](06-reliable-and-measurable-execution.md) | Long-running company work is recoverable, observable, and evaluated | Cycle journal, idempotency keys, usage and finance projections |
| [Template lifecycle](07-template-lifecycle.md) | Companies can install and safely adopt versioned template improvements | Nineteen company definitions and content validation |
| [Company commerce](08-company-commerce.md) | Companies can discover, hire, sell, and settle work end to end | AgentEconomy port, Agent Cards, A2A, x402, ledger |
| [Continuous company review](09-continuous-company-review.md) | The company proposes evidence-backed improvements without self-modifying | Manager and Change Proposal target design |

## Cross-cutting rules

Every feature in this audit should preserve the following invariants:

1. **The Operator remains accountable.** Irreversible, public, financial, and
   policy-changing actions remain reviewable and auditable.
2. **Company state is durable.** A restart must not lose accepted work,
   approvals, receipts, or configuration provenance.
3. **The manifest remains a root of trust.** Runtime overlays must never
   silently rewrite the version-controlled company definition.
4. **Storage remains portable.** New durable state goes through ports and the
   shared backend conformance suite.
5. **Integrations degrade gracefully.** Optional services must not make the
   default offline build unusable.
6. **Product language stays nontechnical.** Internal concepts are translated
   through `docs/spec/glossary.md` before appearing in the console.
7. **No feature bypasses budgets or approvals.** Background work, workflows,
   teammates, and commerce all use the same policy and ledger boundaries.

## How to mature an audit draft

Before one of these documents becomes an implementation specification, add:

- explicit user journeys and failure journeys;
- normative data models and API contracts;
- migration and compatibility behavior;
- security and privacy analysis;
- observability and cost requirements;
- commit-sized milestones and dependency ordering;
- automated acceptance tests across fs, sqlite, and mongodb where applicable.

Detailed work should land in focused documents rather than allowing any file
in this directory to exceed the repository's 500-line Markdown limit.
