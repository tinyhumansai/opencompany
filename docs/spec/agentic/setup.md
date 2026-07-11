# Agentic Setup — the Architect

The Architect turns a plain-language conversation into a company tailored to
the Operator: a Roster sized to their actual business, checkpoints matched
to their risk tolerance, schedules matched to their rhythm. It replaces
"pick one of 18 templates" as the *only* path while keeping templates as
priors and as the offline fallback. Overview and invariants:
[README.md](README.md).

## Position in the lifecycle

The Architect runs while a company is in the `onboarding` state
([runtime/lifecycle.md](../runtime/lifecycle.md)) and generalizes the
[onboarding interview](../company-brain/charter.md): where the interview
fills four Charter fields on top of a fixed template, the Architect designs
the whole effective configuration — Charter, Roster, `[policy]`,
`[[schedule]]`, draft `[place].skills` — and the interview questions become
one part of its conversation.

It is a cognition job on the [`Brain` port](../runtime/ports.md): a
dedicated onboarding session against hosted Medulla, with the Template
library loaded as context. It registers **no effect-producing tools** — its
only output channel is a Blueprint handed back to the runtime. It cannot
send messages, spend money, or touch tiny.place.

## The Blueprint

A Blueprint is the Architect's sole artifact — a complete draft company:

```text
Blueprint
├── manifest      a full company.toml (runtime/manifest.md schema)
├── charter       mission, tone, never_do, services — the interview fields
├── rationale     one plain-language sentence per major decision:
│                 each teammate ("why this role"), each checkpoint,
│                 each schedule, the budget suggestion
└── provenance    which template(s) seeded which part, if any
```

Normative rules:

- The manifest MUST pass `opencompany check`, including every
  [template lint rule](../product/templates.md): unique agent ids,
  `[policy]` present, `human_role` stated, priced skill entries. An invalid
  Blueprint is never shown to the Operator; the runtime rejects it and the
  Architect retries.
- Defaults MUST be conservative regardless of what the conversation said:
  `[policy].mode = "supervised"`, all money/publish/filing effects in
  `always_approve`, `[place].discoverable = false`, `[brain]` defaults. The
  Operator can loosen these later as growth moments — the Architect never
  pre-loosens them.
- `[budget].monthly_usd` is a **suggestion field** in the Blueprint: the
  Operator MUST type or explicitly confirm the number at the launch review;
  it is never silently defaulted from conversation inference.
- The rationale is part of the artifact, not decoration: it is stored with
  the Blueprint and later shown as provenance ("this teammate exists because
  you said you spend Sundays on invoices").

## The conversation

Five movements, each skippable, none technical:

1. **Describe** — "Tell us about your business in your own words." Follow-up
   questions cover customers, offerings and prices, volume, channels, and
   the classic interview trio (mission, tone, never-do).
2. **Draft** — the Architect composes a Blueprint. Templates act as priors:
   it may start from one, blend several (the photographer gets the design
   studio's production roles plus the accounting firm's invoicing teammate),
   or compose from scratch. Selecting a template card in the gallery is
   still supported and simply pins the prior.
3. **Walkthrough** — the draft is presented in prosumer language: the team
   (roles in plain words), **what you keep**, what will always ask first,
   the weekly rhythm, and the suggested budget. Every line links to its
   rationale.
4. **Revise** — the Operator reacts in chat ("I don't want it talking to
   clients", "add someone for bookkeeping") and the Architect re-drafts.
   Each revision is re-validated. There is no revision limit, but the
   onboarding session has the standard cycle budget cap.
5. **Launch** — an explicit approval. The runtime persists the Blueprint's
   manifest and charter (seed layers, per
   [manifest layering](../runtime/manifest.md)), records a
   `BlueprintAccepted` event with the full artifact, and moves the company
   to active. Nothing before this step has any effect on the world.

## Re-architecting after launch

The Architect stays invocable for the company's whole life ("reshape my
company", or a growth-moment entry point in Settings). Post-launch it MUST
NOT emit a fresh manifest — the company has live provenance layers that a
manifest swap would bulldoze. Instead it emits a **batch of
[Change Proposals](proposals.md)** computed as a diff against the effective
configuration, resolved one by one in the Approvals Inbox like any Manager
suggestion. The conversation flow is identical; only the output type
changes.

## Fallback and degradation

- **No brain / no key**: setup falls back to the static path — Template
  Gallery, name it, classic four-question interview. The Blueprint concept
  still applies (the template *is* the Blueprint, rationale = template
  README), so downstream surfaces don't branch.
- **Mid-conversation brain loss**: the partial conversation is journaled;
  resuming re-enters at the last movement. The Operator can bail to the
  static path at any time without losing the name or answers already given.
- **Explore mode** (no key): the Architect is unavailable; template cards
  carry a "with a key, we tailor this to you" affordance.

## Platform mode

Platform operators ([product/platform.md](../product/platform.md)) can
drive the same flow headlessly: `POST /api/v1/companies` accepts either a
finished manifest (today's contract, unchanged) or an Architect brief
(freeform business description), returning a Blueprint for programmatic
review before activation. The review step MUST NOT be skippable — the
provisioning API returns the Blueprint and requires a second call to accept
it.
