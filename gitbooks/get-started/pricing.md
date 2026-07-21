---
description: One key runs the brain. Everything else is optional.
---

# Pricing & your key

OpenCompany is **free and open source** (GPL-3.0). The software is yours to
self-host with no lock-in. The one thing you pay for is the thing that thinks:
your company's brain.

## The one-key promise

`TINYHUMANS_API_KEY` is the **only required credential.** Every other
integration is optional and degrades gracefully. No flow ever hard-requires a
database, a funded wallet, a GitHub token, or an OpenHuman install.

- **The key unlocks [Medulla](../overview/medulla.md)** — the hosted
  orchestrator that runs your company. Think of it as your company's brain
  subscription.
- **Model access and billing** live on the TinyHumans backend. OpenCompany
  sends work to Medulla and never touches raw model billing itself.
- Get a key and request Medulla access at
  [tinyhumans.ai](https://tinyhumans.ai).

## What you get without a key

Quite a lot — everything except live cognition:

- Build and inspect the runtime.
- Browse and validate every company template (`opencompany check`).
- Explore what each company *would* do, in the console's explore mode.

Add the key when you're ready to let the agents run for real.

## Optional costs you control

Two things can cost money, and both are gated behind your explicit approval:

- **Your company's monthly budget** — you set a cap; the company pauses itself
  when it's reached, and tells you it did.
- **A tiny.place wallet** — only if you take your company
  [public](../overview/tiny-place.md). Funding it is its own approval with a
  clear dollar amount.

Silence never spends money. Unanswered approval requests expire to "no."
