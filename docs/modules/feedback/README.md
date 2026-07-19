# Feedback Module

The feedback module implements the [feedback loop](../../spec/feedback-loop/README.md):
in-product feedback reaches the TinyHumans hub or the public issue tracker, with
a non-negotiable privacy gate in between.

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
- the **hub client** (`tinyhumans.rs`) — a mockable `TinyHumansClient` (real
  HTTP client behind the optional `tinyhumans` feature). On a provisioned
  instance it forwards the scrubbed report to the backend's
  `POST /feedback/ingest`, where it is recorded on behalf of the credential's
  owner, and the filer above is skipped.

The scrub-then-preview gate returns the exact, byte-for-byte final issue body;
nothing is transmitted without confirmation or standing per-category consent,
and both destinations receive that identical body. Capture routes:
`POST /api/v1/companies/{id}/feedback`, a built-in `feedback` tool, and an
operator-chat intent. `GET` on the same path lists past reports as the
`FeedbackSummary` projection, which omits the operator's local-only words.
