# Shared skills library

General-purpose skills any company can install, independent of a single
company's domain. Company-specific skills live in
`companies/<name>/skills/`; these are the shared registry.

Each skill is a directory with a `SKILL.md` — YAML frontmatter (`name`,
`description`, and an optional `category` and `version`) followed by the
write-up (When to use / Steps / Output). This is the same format used per
company, so skills move between the two without change. `category` groups a
skill in the console's Skills view against the
`Marketing / Research / Ops / Content / Finance` set.

`version` records the revision a skill ships. Installing a skill snapshots this
whole file into the company, `version` included, so an install is pinned to the
revision it was made from. Bump it when you change a skill's procedure. Every
skill here carries one (a test enforces that), so add `version` to any new
skill.

The console's registry tab lists this directory live, over
`GET …/skills/registry`. Installing resolves the slug against this library
server-side and stores the document verbatim — the client cannot supply skill
content, and installing a slug that is not here fails with `404`.

| Skill | Category | What it does |
| --- | --- | --- |
| [web-research](web-research/SKILL.md) | — | Answer a question from multiple sources with citations. |
| [weekly-report](weekly-report/SKILL.md) | — | Compile the week's activity into a short report. |
| [cold-outreach](cold-outreach/SKILL.md) | Marketing | Personalized first-touch messages that earn a reply. |
| [seo-audit](seo-audit/SKILL.md) | Marketing | Audit a site's organic-search health into ranked fixes. |
| [landing-page](landing-page/SKILL.md) | Marketing | Build and A/B test a conversion-focused landing page. |
| [competitor-scan](competitor-scan/SKILL.md) | Research | Profile competitors and surface where you can win. |
| [deal-memo](deal-memo/SKILL.md) | Research | Turn diligence into a memo with a recommendation. |
| [meeting-brief](meeting-brief/SKILL.md) | Ops | A one-page brief so the operator walks in ready. |
| [call-debrief](call-debrief/SKILL.md) | Ops | Turn a call transcript into decisions and owned action items. |
| [customer-followup](customer-followup/SKILL.md) | Ops | A timely, personal follow-up that moves a thread forward. |
| [hiring-screen](hiring-screen/SKILL.md) | Ops | Screen a candidate against a role into a recommendation. |
| [changelog-writer](changelog-writer/SKILL.md) | Content | Turn merged changes into a user-facing changelog. |
| [social-calendar](social-calendar/SKILL.md) | Content | Plan a two-week social calendar of post-ready slots. |
| [invoice-drafting](invoice-drafting/SKILL.md) | Finance | Draft an accurate, itemized invoice ready to send. |
| [expense-report](expense-report/SKILL.md) | Finance | Compile receipts into a reconciled expense report. |
