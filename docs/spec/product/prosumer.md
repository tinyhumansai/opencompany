# The Prosumer Journey

The golden path for a non-technical operator, install to earnings. Every
screen and error message on this path follows the
[prosumer translation table](../glossary.md) — this doc deliberately uses
the words the operator will see.

## 1. Install

One download: the desktop app (the Tauri shell — whether it ships as an
OpenHuman mode or a dedicated frontend is an open question; the runtime API
is identical) or the `opencompany` binary for the terminal-comfortable.
Nothing to provision: storage is a folder
([runtime/lifecycle.md](../runtime/lifecycle.md), fs bundle).

## 2. Connect

Paste the TinyHumans key — framed as *"your company's brain subscription."*
Validation happens immediately with plain-language failures ("that key
didn't work — check for a missing character" / "your subscription is out of
credit"). Without a key the app still opens in explore mode: browse
templates, read what each company would do.

## 3. Pick a template

The 18 example companies presented as a **Template Gallery** — Marketing
Agency, Law Firm, Design Studio, … ([templates.md](templates.md)). Each card
shows: what the company produces, the team it comes with (roles in plain
words), and **what you keep** (the human role — sign-offs, taste, client
relationships).

## 4. Name it and interview

The operator names the company; the brain runs a five-minute onboarding
interview ([charter](../company-brain/charter.md)): who your customers are,
what you charge, what it must never do without asking. Every question is
skippable; skipped answers keep sensible template defaults.

## 5. Go live

The company starts working. Daily life happens on three surfaces:

- **Chat** — talk to the company like a cofounder. One voice; asking "who
  did this?" gets a plain answer ("our copywriter draft, reviewed by the
  brand lead") without exposing machinery.
- **Work Feed** — a running plain-language log: "Drafted the May newsletter
  (attached). Scheduled 3 posts. Two invoices went out."
- **Approvals Inbox** — anything irreversible waits here with the full
  context and an approve / deny / edit control
  ([approvals](../company-brain/approvals.md)). Unanswered requests expire
  to "no" — silence never spends money.

Notifications: approvals and money events notify immediately; everything
else digests daily by default.

## 6. Growth moments

Each is a single plain-language decision in Settings:

- **Go public** — "List my company on tiny.place so other companies can
  hire it." Triggers the [going-public flow](../company-as-agent/README.md):
  claiming the paid @handle, publishing the services card, opening the jobs
  endpoint — each step its own approval, including funding the wallet with a
  clear dollar amount.
- **Add a channel** — "Let the company answer its own email." (Delegates to
  OpenHuman channels; degrades with a plain warning if unavailable.)
- **Loosen the fence** — "Stop asking me about spending under $5."
  Compiles to a standing rule with visible history
  ([approvals](../company-brain/approvals.md)).
- **Say something was wrong** — thumbs-down on any piece of work opens the
  [feedback flow](../feedback-loop/README.md): the operator previews the
  exact scrubbed text before anything is filed publicly.

## Failure states, in plain words

| Condition | What the operator sees |
| --- | --- |
| Brain unreachable | "The company can't think right now — we're reconnecting. Nothing is lost." |
| Budget cap hit | "Your company paused itself: it reached the $200 monthly limit you set." |
| tiny.place down | "Jobs from other companies are paused; your own work continues." |
| Tool unavailable | "Email isn't connected yet — here's the draft to send yourself." |
