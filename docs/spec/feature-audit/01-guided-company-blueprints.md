# Guided Company Blueprints

## Outcome

An Operator can describe a business in plain language, receive a complete and
validated draft company, understand why each major choice was made, revise it,
and explicitly approve it before anything runs.

## Why this matters

The repository already provides a strong template library, but selecting and
editing a company definition still assumes that the Operator understands
rosters, schedules, policies, budgets, and integrations. The product promise is
stronger when setup begins with the business rather than its configuration.

The target design for the Architect and Blueprint already lives in
`docs/spec/agentic/setup.md`. The missing feature is the complete product and
runtime path that turns that design into the normal onboarding experience.

## Proposed capability

- Start from a freeform description, a selected template, or a blend of both.
- Ask only the follow-up questions required to resolve customers, services,
  channels, risk tolerance, schedules, and retained human responsibilities.
- Produce a Blueprint containing the manifest, charter, rationale, provenance,
  proposed integrations, and a budget suggestion.
- Validate every draft with the same rules as `opencompany check`.
- Present a plain-language walkthrough of the team, recurring work, approvals,
  expected connections, budget, and what the Operator keeps.
- Support iterative revision without applying partial configuration.
- Require explicit confirmation of budget and launch.
- Persist the accepted Blueprint and its rationale as lifecycle provenance.
- Fall back to static template onboarding when the preferred brain is
  unavailable.
- Expose the same review contract through platform provisioning without
  allowing headless callers to skip acceptance.

## Product surfaces

- A first-run setup flow in the console.
- A review screen with team, responsibilities, approvals, schedule, budget,
  required connections, and rationale.
- A resumable onboarding conversation.
- A provisioning API that separates draft generation from activation.
- A post-launch “reshape my company” entry point that emits Change Proposals
  rather than replacing the live manifest.

## Acceptance boundary

- No external effect may run during Blueprint generation.
- Invalid drafts are rejected before presentation.
- Conservative approvals and private discovery are the defaults.
- The Operator must explicitly accept the Blueprint and budget.
- Onboarding survives process restart and resumes from the last durable step.
- The accepted artifact records template and interview provenance.
- Static onboarding remains usable without an active model connection.

## Likely implementation seams

- `src/company/manifest.rs` and `src/company/types.rs`
- `src/runtime/lifecycle.rs` and `src/runtime/builder.rs`
- `src/server/provision.rs` and a focused onboarding router
- `src/ports/` for durable Blueprint storage if it does not fit CompanyStore
- `frontend/` for the setup, review, revision, and activation flow
- `docs/spec/agentic/setup.md` as the normative behavior source

## Open questions

- Whether drafts live inside CompanyStore or a dedicated onboarding port.
- How long abandoned onboarding sessions are retained.
- Which integration checks can run before launch without requesting secrets.
- How Blueprint schema versions migrate independently of manifest versions.
