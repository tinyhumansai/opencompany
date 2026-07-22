# E2E Harness Co

> A three-agent company used by the frontend end-to-end suite
> (`frontend/test/e2e`). It mirrors `companies/openhuman_demo` and adds a
> `[users] admins` list so a test can sign in through the real magic-link flow
> and drive the operator console against a live host.

## Why it exists

The Playwright wiring spec proves the full operator chain end to end:

```
auth (magic link) → console → POST /api/v1/company/chat → brain → reply bubble
```

To do that it needs an address that is allowed to log in. `[users] admins`
lists `harness-e2e@tinyhumans.ai` as a standing admin invite, so the test can
request a login code, redeem it for a session cookie, and then open
`#/conversation`.

## Agent roster

| Agent | Responsibility |
| --- | --- |
| Chief Executive | Sets direction, answers about the company, delegates the work. |
| Engineer | Explains how things are built and proposes technical plans. |
| Writer | Turns rough notes into short, clear written drafts. |

## Running it

The default (offline) build boots it on the echo brain, which is enough for a
manifest load check:

```bash
cargo run --bin opencompany -- serve --company companies/e2e_harness \
  --bind 127.0.0.1:8080
```

Built with `--features openhuman` and an inference credential, each agent runs
on the embedded OpenHuman runtime instead. The e2e suite points a mocked LLM
backend at it and asserts the reply renders in the console.
