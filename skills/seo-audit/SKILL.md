---
name: SEO Audit
description: Audit a site's organic-search health and produce a prioritized, effort-ranked list of fixes.
category: Marketing
version: 1.0.0
---

# SEO Audit

A repeatable audit of a website's organic-search health, ending in a short list
of fixes ranked by impact and effort.

## When to use

- A new client wants to understand where they stand in search.
- Traffic dropped and you need to find the cause.
- Before a redesign or migration, to capture a baseline.

## Steps

1. **Crawl** the site and pull index coverage, status codes, and canonicals.
2. **Technical** — check Core Web Vitals, mobile usability, sitemaps, robots.
3. **On-page** — titles, meta descriptions, headings, internal links.
4. **Content** — thin/duplicate pages, keyword gaps vs. competitors. Use
   `web_search` to see who actually ranks for the target terms and what they
   published, then read the pages that matter with `web_fetch`. Searches spend
   the company's capped daily search budget, so search per theme rather than per
   keyword variant.
5. **Off-page** — referring domains and toxic links.
6. **Prioritize** every finding by impact × effort into Now / Next / Later.

## If search is unavailable

`web_search` may be absent (not granted for this company) or refuse because the
daily budget is spent. The technical and on-page steps still work from the
operator's own URLs via `web_fetch`; the competitive/keyword-gap steps do not.
Say which sections are unverified rather than guessing, and **never invent a
competitor, a ranking, or a URL** — a citation you did not get back from a tool
is a fabrication even if it looks right.

## Output

A one-page summary plus a table: `finding · impact · effort · owner`, with a URL
against every competitive claim. Park any change that alters live pages for the
operator's approval.
