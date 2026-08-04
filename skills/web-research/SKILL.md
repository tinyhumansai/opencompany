---
name: Web Research
description: Answer a question from multiple independent sources and return a cited, verified summary.
version: 1.0.0
---

# Web Research

Research a question across several sources, verify the claims, and return a
short, cited answer.

## When to use

- A decision needs facts you don't already have.
- You need current information beyond what's in memory.

## Tools

Discovery and retrieval are separate steps with separate tools:

- `web_search` **finds** sources — it returns titles, URLs, domains, dates and
  short snippets. Each call spends the company's search budget and the budget is
  capped per day, so search once with a good phrase rather than repeatedly with
  variations.
- `web_fetch` **reads** one page you already have a URL for. Snippets are
  previews, not the page.

## Steps

1. **Frame** the question and what a good answer looks like.
2. **Search** with `web_search` to gather several independent sources.
3. **Read** the primary sources with `web_fetch` — not just the snippets.
4. **Verify** — corroborate each key claim across two sources.
5. **Synthesize** a short answer; cite every claim with a URL you actually saw
   in a `web_search` result or fetched.

## If search is unavailable

`web_search` may be absent (this company has not granted it) or may refuse
because the daily budget is spent. Either way:

- **Say so, plainly, in your answer.** "I could not search the web" is a
  complete and useful sentence.
- Work from URLs the operator supplied, reading them with `web_fetch`.
- **Never fabricate a citation.** Do not write a URL you have not seen returned
  by a tool, and do not attribute a claim to a source you did not read. An
  unverified answer that says it is unverified is worth far more than a
  confident one with invented links.

## Output

A concise summary, a bullet list of findings with links, and an explicit note
on anything that could not be verified.
