# 08 — WS8: Skills Library & Company Starter Content

## Scope

Content, not code. Two gaps:

1. The **shared skills library** (repo-level `skills/`) has only
   `web-research` and `weekly-report` — the console's install registry needs
   a real catalog.
2. Of the 19 company templates under `companies/`, only
   `agentic_marketing_agency` ships the full directory (skills, workflows,
   workspace). The other 18 have just `company.toml` + `README.md`, so their
   consoles boot empty.

Templates are the productized manifests of
[`docs/spec/product/templates.md`](../spec/product/templates.md) — this
workstream makes each one feel staffed and ready on first boot.

## Design

### Formats (frozen by WS1)

- `SKILL.md`: `---` frontmatter with `name`, `description` (+ optional
  `category`), then `# Title / ## When to use / ## Steps / ## Output`
  sections. Same format for library and company skills — they move between
  the two unchanged, and the body feeds the WS4 harness directly.
- `workflows/<id>.toml`: `id`/`name`/`description` + `[[node]]`
  (trigger/agent/tool_call/http_request/condition/output, `agent` refs a
  roster id) + `[[edge]]`. Every workflow referenced from
  `[workflows].enabled` must exist and validate.
- `workspace/**`: topic folders of Markdown with `[[wikilinks]]`; a
  `README.md` orienting the operator.

All content follows the [glossary](../spec/glossary.md) — prosumer language
only.

### Shared library target (~12 skills)

Category-balanced against the console's `SkillCategory` set (Marketing,
Research, Ops, Content, Finance):

`cold-outreach`, `competitor-scan`, `meeting-brief`, `invoice-drafting`,
`expense-report`, `seo-audit` (promoted from the marketing company),
`landing-page` (likewise), `changelog-writer`, `social-calendar`,
`customer-followup`, `hiring-screen`, `deal-memo` — plus the existing
`web-research` and `weekly-report`. Each ≤80 lines, concrete steps, one
clearly stated output.

### Per-company starters (18 companies)

For each bare company, using `agentic_marketing_agency` as the template:

- **2–4 skills** specific to the business (law firm: `contract-review`,
  `client-intake`; VC: `deal-memo`, `diligence-checklist`; support:
  `ticket-triage`, `kb-article`; …),
- **1 workflow** — the company's core pipeline, agents mapped to its actual
  roster ids,
- **workspace/** — README + 2–3 seed notes (playbook checklist, a brand/voice
  or policies note, one worked example) cross-linked with wikilinks,
- `[workflows].enabled` updated in `company.toml`.

Batches (one subagent each, zero file overlap):

1. Professional services: law, accounting, consultation, recruiting.
2. Product/tech: software, design studio, game studio, game business, pharma.
3. Go-to-market: enterprise sales, customer support, influencer, media.
4. Capital: VC, venture studio, real estate, accelerator misc
   (`signals_opportunity_studio`, `startup_accelerator`).

### The guardrail

WS1's content-validation test (`companies/*` + `skills/*` walk) runs in CI —
malformed content fails the build, so authored content can never drift from
the parsers.

## Subtasks

1. `feat(skills): expand the shared library to ~12 skills`
2. `feat(companies): starter content — professional services batch`
3. `feat(companies): starter content — product/tech batch`
4. `feat(companies): starter content — go-to-market batch`
5. `feat(companies): starter content — capital batch`

## Dependencies

WS1 (format freeze + validation test). Nothing depends on WS8 — it can land
any time after, in parallel with everything.

## Tests & exit criteria

Content-validation walk green over every directory; each new SKILL.md renders
correctly in the console Skills view and each workflow renders on the canvas
(manual spot-check per batch, noted in the PR). `opencompany check
companies/<name>` passes for every touched company.
