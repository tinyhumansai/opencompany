---
name: Competitor Scan
description: Profile a handful of competitors and surface where you can win, in one comparison table.
category: Research
version: 1.0.0
---

# Competitor Scan

Build a current picture of the competitive field and turn it into a short list
of openings you can act on.

## When to use

- Before a launch, pitch, or pricing change.
- A new rival appears and you need to understand the threat quickly.

## Steps

1. **Pick** the three to five competitors that actually matter to this decision.
2. **Find** their pages with `web_search` — pricing, product and newsroom pages
   are usually one search each. Each search spends the company's capped daily
   search budget, so search per competitor, not per question.
3. **Gather** positioning, pricing, features and recent moves by reading those
   pages with `web_fetch`. Snippets are previews; a pricing claim needs the page.
4. **Compare** on the axes your buyers care about, not every feature.
5. **Find gaps** — where they are weak, silent, or overpriced.
6. **Recommend** two or three openings ranked by how winnable they are.

## If search is unavailable

`web_search` may be absent (not granted for this company) or refuse because the
daily budget is spent. Then: say so in the deliverable, work only from URLs the
operator gave you, and mark every axis you could not verify as unknown.
**Never invent a competitor's price, feature or funding event, and never write a
URL you have not seen returned by a tool.** A table with honest gaps is usable;
a table with plausible fabrications is not.

## Output

A one-page comparison table plus a short "where we win" note listing the ranked
openings and the evidence behind each. Every factual cell carries the URL it
came from.
