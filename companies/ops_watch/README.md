# Ops Watch

A company whose entire job is to notice that production broke, before a person
does — and to stay quiet the rest of the time.

It is the smallest bundle here, on purpose. Every other vertical produces
something: a brief, a campaign, a portfolio. This one produces silence, and
speaks only when silence would be wrong.

## What it does

Every hour, on the hour, it asks production a fixed set of questions, compares
the answers against the previous hour's, and reports **only what changed**.

| Previously | Now | Posted |
| --- | --- | --- |
| absent | present | 🔴 new |
| present | present, worse | 🟠 degraded |
| present | present, same | nothing |
| present | absent | 🟢 cleared |

Once a day at 09:00 it posts an all-clear whether or not anything happened —
because a monitor that only speaks when something is wrong is indistinguishable
from a monitor that has stopped, and the difference is otherwise discovered
during an incident.

## What it reads, and how it cannot do anything else

Everything this company knows comes from one MCP server, declared in
`mcp.json` as **`k8s-monitoring`**: read-only production Kubernetes, Prometheus
and Grafana. Nine questions, each bounded and paged — cluster overview,
unhealthy pods, one workload's status, what is close to a resource limit,
recent events, Grafana alerts, a dashboard's current numbers, a curated metric
query, and whether the service answers from outside.

The server holds the cluster credential itself, so no credential ever reaches
an agent. It has no write verb, no exec, no Secret read and no unbounded log —
absent from its code and from its cluster role independently, so neither one
alone is load-bearing. Source and specifications:
[`tinyhumansai/k8s-monitoring-mcp`](https://github.com/tinyhumansai/k8s-monitoring-mcp).

`k8s-monitoring` **ships disabled** and names an `authSecret`. Point it at your
own deployment, put a tenant bearer at `mcp/k8s-monitoring/auth`, and enable
it. The first card on the board walks through this.

The company grants no `web`, no `search`, no `shell` and no `code`. Every fact
it reasons from is a tool call against our own cluster; a desk that could reach
the internet would answer about Kubernetes in general instead of about this
cluster, and a teammate with a shell would walk around the read-only boundary
that makes the rest of this safe.

## Roster

| Agent id | Role | Responsibility |
| --- | --- | --- |
| `health_watch` | Health Watch (orchestrator) | Takes the hourly reading. Decides nothing. |
| `incident_analyst` | Incident Analyst | Compares this reading against the last one. |
| `runbook_keeper` | Runbook Keeper | Owns the thresholds; writes the finding. |

The first two split on purpose: a reading taken by whoever is about to
interpret it is, reliably, the reading the interpretation needed. The third
holds no cluster grant, so its judgements are about the record rather than
about a fresh look.

One desk, **Ops desk** — a finding discussed somewhere the person who must act
on it is not, is a finding that did not arrive.

## Ledgers

- **`conditions`** — one row per thing wrong with production, opened when it
  starts and closed when it clears. This is what makes "report once" possible:
  the rule is a comparison, and a comparison needs somewhere the last answer
  was written down.
- **`thresholds`** — what counts as broken: the number, what it is measured
  against, and why that number. Rows rather than prose, because a threshold
  gets argued with and the argument is the valuable part.

## Skills

- **Health Thresholds** — turn a reading into a verdict, or into nothing.
- **State Comparison** — the rule that decides whether this company speaks.
- **Finding Format** — write something actionable from a phone, at night.

## The limit it does not paper over

The monitoring server runs inside the cluster it watches. That is what lets it
see pod-level detail at all, and it means **a cluster-wide outage silences this
company at the moment it is most needed**. Depth and independence want opposite
placements; this one chose depth.

The answer is not to move the pod but to pair it with a small check from
outside, on different infrastructure, watching for the hourly run going
missing. Until that exists this is hourly depth and no independent liveness —
stated here, in the workspace, and in the daily post, rather than discovered.
