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
