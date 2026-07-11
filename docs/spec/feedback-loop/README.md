# Feedback Loop

The product improves through its users: feedback captured in-product becomes
**public GitHub issues** on `tinyhumansai/opencompany`, triage clusters them
into roadmap items, and releases tell the users who spoke up that they were
heard. This is the [vision's](../vision/README.md) learning loop, made
concrete.

Supporting docs: [privacy.md](privacy.md) (normative scrubbing rules),
[triage.md](triage.md) (labels and closure).

## The loop

```text
operator reaction ─▶ Feedback Item ─▶ scrub ─▶ preview ─▶ GitHub issue
        ▲                                                     │
        │                                              triage / cluster
        │                                                     │
"2 things you flagged were fixed in v0.4" ◀── release ◀── roadmap item
```

## Capture

Every brain reply, deliverable, and approval request carries a lightweight
reaction affordance: thumbs-down, "this was wrong", or free text. Capture
also works via `POST /api/v1/companies/{id}/feedback`
([runtime/api.md](../runtime/api.md)), an operator-chat intent ("that
invoice was wrong — flag it"), and a built-in `feedback` tool the brain
itself can invoke when the operator complains mid-conversation.

A **Feedback Item** snapshots: category, the operator's words, the work item
it concerns, template name + version, runtime version, and a *redacted*
context excerpt. Items persist in the company's feedback family
([company-brain](../company-brain/README.md)) whether or not they are ever
filed.

## Consent modes

| Mode | Behavior | Default |
| --- | --- | --- |
| **manual** | Operator files via a prefilled issue link; nothing leaves the machine otherwise | ✔ default |
| **assisted** | The company drafts the issue; the operator taps approve on the exact final body | opt-in |
| **auto** | Standing consent per category ("file template gaps without asking"); revocable anytime; still journaled | opt-in |

In every mode the **scrub-then-preview** gate of
[privacy.md](privacy.md) applies: the operator (or their standing consent)
sees exactly what becomes public. Filing without `GITHUB_TOKEN` degrades to
the manual prefilled link ([runtime/config.md](../runtime/config.md)).

## Agent-filed issues

Assisted/auto filings are posted by a shared bot account and **signed with
the company's @handle** in the issue body for provenance (verifiable against
the tiny.place directory). Obligations: dedupe against existing issues
before filing (search first, comment instead of duplicating), rate-limit per
company, and label `source/agent-filed` so triage can weight accordingly.

## Closing the loop

- Filed issues' URLs land back in the Feedback Item; a scheduled poll tracks
  status changes into company memory.
- Release notes map fixes → originating issues
  ([triage.md](triage.md)); the runtime surfaces this in-product: *"2 things
  you flagged were fixed in v0.4"*, and the bot comments on the issues.
- The loop is measured: time-to-triage, cluster-to-roadmap rate, and
  fixed-feedback-per-release are the health metrics of the product itself.
