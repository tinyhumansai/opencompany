---
description: What separates a company that runs from a pile of chatbots.
---

# Why it works

Plenty of tools can call a model. Very few can run a business without you in
every message. Here's what makes the difference.

## A real org chart, not a prompt

Each company is declared as a roster of agents with distinct mandates in a
simple `company.toml`. The host instantiates them, coordinates them, and keeps
them running. You're configuring an organization, not babysitting a chat
window.

## Humans in the loop where it counts

Every template names the exact decisions reserved for you — spend, send, sign,
publish. Delegate the work; keep the judgment. Anything irreversible waits in
the **Approvals Inbox** with full context, and **silence expires to "no"** so
nothing risky happens by default.

## Built on proven runtimes

OpenCompany is a light host over **OpenHuman** and the **TinyHumans** agent
modules. It reuses their runtime — tools, channels, credentials, orchestration
— instead of reinventing it. Less surface area to trust, more capability out of
the box.

## Rust-fast and inspectable

An Axum HTTP surface, a small default build, and deeper capabilities behind
feature flags. Simple to start, honest to operate, easy to test. You can read
what it does and watch it do it.

## Degrades gracefully

The only thing OpenCompany truly requires is a single API key. Storage is a
folder by default. tiny.place is opt-in. GitHub, email, databases, funded
wallets — all optional. When something is unavailable, you get a plain warning
and a draft to handle yourself, never a dead end.

| If this breaks… | You see… |
| --- | --- |
| The brain is unreachable | "The company can't think right now — reconnecting. Nothing is lost." |
| You hit your budget cap | "Your company paused itself: it reached the limit you set." |
| A tool isn't connected | "Email isn't connected yet — here's the draft to send yourself." |

## Yours to own

GPL-3.0, self-hostable, no lock-in. Your company's brain state is a durable
store you control.

Next: meet [Medulla, the engine](medulla.md).
