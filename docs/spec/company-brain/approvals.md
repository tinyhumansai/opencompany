# Checkpoints and Approvals

The trust core of the product: agents act freely inside the fence; anything
irreversible waits for the Operator. This doc is normative.

## Checkpoint taxonomy

Effect kinds that MAY require sign-off, grouped by what they risk:

| Group | Effect kinds (examples) | Default in `supervised` mode |
| --- | --- | --- |
| **Spend** | `payment.send`, `subscription.start`, x402 outbound above cap | approval above `auto_approve_under_usd` |
| **Send** | `email.send`, `dm.external`, any first message to a new counterparty | approval for new counterparties; allowed for established threads |
| **Sign** | `filing.submit`, `contract.accept` | always approval |
| **Publish** | `external.publish`, Agent Card / price changes, website deploys | always approval |
| **Hire** | outbound A2A engagement with a new company; firing a vendor | approval above threshold or first-time counterparty |
| **Identity** | handle registration/renewal, key rotation, delegated signer mint/expand | always approval |

`readonly` mode gates *every* effect; `full` mode auto-allows everything
except `[policy].always_approve` entries. Modes mirror OpenHuman's security
tiers so an OpenHuman-backed `ApprovalGate` maps 1:1.

## Approval lifecycle

```text
effect emitted ─▶ evaluate ─▶ Allow ─▶ execute, journal
                      │
                      ├─▶ Deny ─▶ returned to brain as refusal (it replans)
                      │
                      └─▶ RequireApproval ─▶ park (ApprovalId)
                                              │  surfaces in approvals inbox + chat
                                              ▼
                            operator resolves: approve │ deny │ edit
                                              │
                                              ▼
                            ApprovalResolved event ─▶ follow-up cycle
```

- **Default-deny on silence**: parked approvals expire (default 7 days,
  configurable) to `deny`. Nothing irreversible ever happens because the
  Operator was on vacation.
- **Edit** lets the Operator amend the effect payload (fix the email, lower
  the amount) and approve the amended version; the brain sees both the
  original and the edit.
- Resolution requires operator auth ([runtime/api.md](../runtime/api.md));
  the resolving `Actor` is journaled.
- Approve executes the parked effect exactly once
  (journal-before-execute, [runtime/lifecycle.md](../runtime/lifecycle.md));
  deny feeds the refusal back so the brain replans rather than retries.
- **Resolution is idempotent.** Resolving an approval that is no longer parked
  — a double-submit, a retried request, two operators on the same queue —
  is a no-op with a fixed reply. It writes no journal record and runs no
  follow-up cycle.

## Approving a blocked tool call: single-use grants

Two different things park on this queue and they need opposite treatment.

A **native** effect is one the runtime performs itself (an email, a workflow
delivery). Approving it executes it, as above.

A **tool call** is one an agent tried to make and the OpenHuman tool policy
blocked. There is nothing for the runtime to execute — the parked effect's
payload is the tool's *arguments*, and only that agent can run the tool. So
approving it mints a **single-use grant** and re-dispatches the agent to
re-issue the call. Without this, approving recorded a verdict and nothing ran:
the operator had to go back and ask for the same thing again.

A grant is:

- **single-use** — redeeming consumes it, so one approval buys one call and
  never standing permission;
- **agent-scoped** — a grant minted for one desk does not admit another's
  identical call;
- **argument-exact** — matching is whole-value equality on the arguments. A
  re-issue with a different recipient or amount does not match and re-parks,
  because the operator never saw those arguments. Approve-with-edit mints
  against the *amended* arguments;
- **time-boxed** — 15 minutes. An approval is consent to act now, not a
  standing authorisation. An unredeemed grant expires and the operator is told
  the agent did not act, so re-approving is an informed choice.

The lifecycle is journaled (`ApprovalGranted` → `GrantConsumed` | `GrantExpired`)
and replayed on boot, so a restart between approving and re-issuing does not
drop the approval. Consumed and expired grants are folded out on replay: a
resurrected single-use grant would not be single-use.

### Precedence at the tool gate

A tool call is decided in this order:

1. `never_do` hard-deny — **reserved**; the delegation-rule compiler is still a
   Phase-1 stub, so no tool-level arm exists yet. It sits above the grant
   deliberately: a grant is an operator saying yes to one call, `never_do` is
   the company saying not ever, and the standing rule is the one meant to
   survive a socially engineered operator.
2. a live **grant** matching agent + tool + exact arguments → allow, once.
3. `[policy].always_approve` → park.
4. mode dispatch (`readonly` / `supervised` / `full`).

The grant sits **above** `always_approve` on purpose. A tool on that list still
parks the first time, which is what the list is for; but once the operator has
approved that specific call, re-parking it would mean approval authorises
nothing at all for precisely the tools an operator most wants to gate
deliberately. Single-use, exact-args and agent-scope are what keep that
narrow.

Note this is the *tool* gate (`ApprovalPolicy`), which is a different path from
the *effect* gate (`ManifestApprovalGate::evaluate`) the taxonomy above
describes. A harness tool call parks directly and never reaches `evaluate`.

## Delegation levels (standing rules)

Prosumers adjust the fence in plain language, which compiles to policy:

- "Auto-approve spending under $5" → `auto_approve_under_usd = 5.0`
- "Never contact my customers directly" → `never_do` → `Deny` on
  `dm.external` matching the customer list
- "You can post to the blog without asking" → remove `external.publish`
  from `always_approve` for that channel

Standing-rule changes are themselves Charter edits with provenance and audit
([charter.md](charter.md)); loosening a rule takes effect for *future*
effects only.

## Audit

The approval log is immutable: every evaluate decision, park, resolution
(with actor and timestamp), expiry, and execution outcome is an `EventLog`
entry, and money-touching effects additionally journal to the ledger. The
operator surface renders this as plain history ("you approved sending the
Acme invoice on June 2").
