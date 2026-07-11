# Charter

The Charter is the company's constitution: what it is, what it sells, how it
speaks, and what it must never do without asking. It seeds from the manifest
`[company]` table, fills in during the onboarding interview, and stays
editable by the Operator for the company's whole life.

## Schema

Stored in `CompanyStore` as part of the `CompanyRecord`; the manifest is the
seed layer, not the live record ([runtime/manifest.md](../runtime/manifest.md)).

| Field | Source | Purpose |
| --- | --- | --- |
| `name` | manifest `[company].name` | display + Agent Card |
| `output` | manifest `[company].output` | one-line what-we-make |
| `mission` | interview | longer articulation the brain quotes when reasoning |
| `human_role` | manifest `[company].human_role` | what the Operator keeps |
| `services` | interview / `[place].skills` | the catalog: id, description, price — source of truth for the Agent Card |
| `tone` | interview | voice rules for outbound copy |
| `never_do` | interview | hard prohibitions, enforced as `Deny` in the ApprovalGate |
| `spend_caps` | `[policy]`, `[budget]` | standing limits |
| `checkpoint_overrides` | `[policy]` + runtime edits | per-effect-kind approval rules |

## Provenance

Every field records which layer set it: template default ⟵ manifest ⟵
interview answer ⟵ operator edit (later wins). The operator-facing settings
surface shows this provenance ("you changed this on May 3; the template
default was …") and every change lands in the `EventLog` as an auditable
event.

## The onboarding interview

When a company enters `onboarding`
([runtime/lifecycle.md](../runtime/lifecycle.md)), the brain runs a short
plain-language interview: *Who are your customers? What do you charge? What
should I never do without asking you?* Answers populate `mission`,
`services`, `tone`, and `never_do`. The interview is skippable; skipped
fields keep template defaults. Non-AI-savvy phrasing is a requirement, not a
nicety — the interview is most prosumers' first contact with the product.

With [agentic setup](../agentic/setup.md) enabled, the interview is the
conversational core of the Architect's Blueprint flow, which designs the
whole company rather than four fields; this static interview remains the
offline fallback. Post-launch, Charter changes suggested by the company
arrive as [Change Proposals](../agentic/proposals.md), which add one
provenance layer between interview answers and operator edits.

## Enforcement points

The Charter is consulted, not decorative:

- **Dispatch guardrails** — `tone` and `never_do` are injected into the
  brain's prompt variables every cycle.
- **ApprovalGate** — `never_do` compiles to `Deny` rules;
  `checkpoint_overrides` and `spend_caps` drive
  Allow/RequireApproval/Deny decisions ([approvals.md](approvals.md)).
- **Agent Card generation** — `services` deterministically generates the
  card and `skill.md`
  ([company-as-agent/identity.md](../company-as-agent/identity.md));
  publishing a changed card is itself a checkpoint.
- **Delegated signer caps** — `spend_caps` bound every signer minted
  ([company-as-agent/commerce.md](../company-as-agent/commerce.md)).
