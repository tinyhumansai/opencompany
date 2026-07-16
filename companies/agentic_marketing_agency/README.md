# Agentic Marketing Agency

> A full-service agency of agents producing creative, copy, SEO, paid, email, and landing pages — with a human reviewing campaigns before they ship.

## What it can do

- Develop brand strategy and creative concepts.
- Write copy and build landing pages.
- Run SEO and paid-acquisition campaigns.
- Design and send email programs.
- Measure performance and optimize spend.

## Agent roster

| Agent | Responsibility |
| --- | --- |
| Creative Director | Own creative concept and direction. |
| Copywriter | Write ads, pages, and campaign copy. |
| SEO Specialist | Organic search strategy and optimization. |
| Paid Ads Manager | Plan and run paid-acquisition campaigns. |
| Landing Page Builder | Build and test conversion pages. |
| Email Marketer | Design and send lifecycle email. |
| Analytics Analyst | Measure performance and report. |
| Brand Strategist | Positioning and brand strategy. |

## Human in the loop

Humans keep **campaign review and sign-off**; the agents run everything else. The output of this harness is **campaigns across every channel**.

## Company files

Alongside `company.toml`, a company directory can ship starter data that seeds
the operator console:

| Path | Seeds |
| --- | --- |
| `company.toml` | Identity, `[[agent]]` roster, `[[group_chat]]` desks, `[[connection]]` priorities, `[workflows].enabled` |
| `workflows/<id>.toml` | A workflow graph enabled in `company.toml` (one file per workflow) |
| `workspace/**` | The **template workspace** — starter Markdown notes/folders, `[[wiki linked]]` |
| `skills/<id>/SKILL.md` | A **skill** — YAML frontmatter (`name`, `description`) + a write-up (When to use / Steps / Output) |

Every company can define its own `workspace/` and `skills/`; this one ships a
Brand / Campaigns / Playbooks workspace and SEO, landing-page, email, and
brand-positioning skills. Shared, non-company skills live in the repo-level
[`skills/`](../../skills/) library and can be installed into any company.

## Run it

```sh
cargo run --bin opencompany -- serve --company companies/agentic_marketing_agency
```
