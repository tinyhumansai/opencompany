# Product

OpenCompany is the open-source runtime that turns one person into a whole
company: a durable host where the [Brain](../glossary.md) runs a roster of AI
teammates that do the work, sell services to other agents on tiny.place, and
ask the human only for the decisions that matter.

Supporting docs: [prosumer.md](prosumer.md), [platform.md](platform.md),
[templates.md](templates.md).

## Thesis

A single operator brings capital, taste, and irreversible decisions;
everything else — production, marketing, sales, support, books — is a
functional job an agent can hold around the clock. "Done" means:

- **For the prosumer**: their business runs while they sleep, nothing
  irreversible happens without them, and they never needed to learn what an
  agent graph is.
- **For the platform operator**: provisioning a working company is one API
  call, storage is theirs, and upgrades never strand tenant data.

## Personas

| | Prosumer Operator | Platform Operator |
| --- | --- | --- |
| Who | A non-AI-savvy person running a one-person business | A builder embedding or hosting fleets of companies |
| Buys | "OpenCompany is my staff" | "OpenCompany is my company runtime" |
| Touches | Chat, Work Feed, Approvals Inbox, Earnings, Settings | the crate, the provisioning API, storage ports, webhooks |
| Never sees | agent graphs, tiers, cycles, dispatch ([glossary](../glossary.md), normative translation table) | prosumer UI (they build their own) |

## Surfaces

| Surface | Core? | What it is |
| --- | --- | --- |
| **Chat** | core | One conversation with the company as a single entity — the Brain speaks for the whole org |
| **Work Feed** | core | What the team did, in plain language, with artifacts attached |
| **Approvals Inbox** | core | Checkpoints awaiting sign-off: spend, send, sign, publish |
| **Earnings / Ledger** | with tiny.place | Money in/out, jobs sold and bought |
| **Settings** | core | Charter edits, standing approval rules, going public, feedback consent |

## The one-key promise (normative)

`TINYHUMANS_API_KEY` is the only required credential
([runtime/config.md](../runtime/config.md)). Every other integration is
optional and degrades gracefully; no flow may hard-require a database, a
funded wallet, a GitHub token, or an OpenHuman install. A violation of this
promise is a release blocker.
