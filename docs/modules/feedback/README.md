# Feedback Module

The feedback module implements the [feedback loop](../../spec/feedback-loop/README.md):
in-product feedback becomes public GitHub issues, with a non-negotiable privacy
gate in between.

- `types.rs` — the `FeedbackItem` (category, operator words, work item, template
  + runtime version, capped/redacted excerpt) and consent modes
  (`manual` default, `assisted`, `auto`).
- the **scrubber** — enforces the normative redaction classes from
  [privacy.md](../../spec/feedback-loop/privacy.md): any `SecretStore` value or
  key material **aborts** filing; personal data and charter specifics are
  masked; customer content is never included. Scrubbing fails **closed** — if a
  class can't be evaluated, filing is blocked.
- the **filer** — a mockable `GitHubClient` (real REST client behind the
  optional `github` feature); dedupes by searching existing issues first,
  rate-limits per company, signs the body with the company `@handle`, and labels
  `source/agent-filed`. Without `GITHUB_TOKEN` it degrades to a prefilled manual
  issue link.

The scrub-then-preview gate returns the exact, byte-for-byte final issue body;
nothing is transmitted without confirmation or standing per-category consent.
Capture routes: `POST /api/v1/companies/{id}/feedback`, a built-in `feedback`
tool, and an operator-chat intent.
