---
name: Health Thresholds
description: Decide whether a production reading has crossed a line worth waking somebody for.
category: Operations
version: 1
---

# Health Thresholds

A reading on its own is a number. This turns it into a verdict, or into
nothing — which is the more common and more valuable answer.

## When to use

- Every hourly run, after the reading is taken and before anything is reported.
- When proposing or revising a `thresholds` row.

## Steps

1. **Read the thresholds**, do not recall them. They are rows, they change, and
   the version in your head is the version from whenever you last looked.
2. **Match each reading to its line.** A reading with no threshold behind it is
   a judgement call — allowed, but say so, because a judgement call is not
   repeatable and the next run will not make it the same way.
3. **Check the denominator.** 400 millicores is fine against a limit of 2 and an
   emergency against a limit of 0.5. A figure with no denominator is not a
   verdict, it is a number.
4. **Separate unmeasured from fine.** A question that failed, a metrics server
   that is down, a volume with capacity but no usage — none of these are
   all-clears. They are gaps, and they go in the reading as gaps.
5. **Hold the line for its full window.** "Above 90% of its limit for the whole
   hour" is not the same as "above 90% once". A threshold with a window is not
   crossed until the window is.

## The two verdicts

**Resource exhaustion** — any of: a node reporting `MemoryPressure`,
`DiskPressure` or `PIDPressure`; a PVC at or above 80% used; Redis
`used_memory / maxmemory` at or above 0.75; a node or pod above 90% of its
limit for the whole hour.

**Service downtime** — any of: a deployment below desired replicas for more
than five minutes; an external probe down; a 5xx rate above 5%; the outside-in
check red.

Plus the two patterns that produced the incidents this company was built
after: WebSocket connections down more than 50% hour-over-hour, and upstream
429s on the managed model.

## Output

For each reading: crossed, not crossed, or could not be measured — with the
number and the line it was measured against. Feeds
[[State Comparison]], which decides whether anybody hears about it.
