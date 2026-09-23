---
name: State Comparison
description: Report a condition when it starts and when it clears, and never while it sits there unchanged.
category: Operations
version: 1
---

# State Comparison

The rule that decides whether this company speaks. It is the difference
between a monitor people read and a monitor people filter into a folder.

## When to use

- Every hourly run, after the thresholds have been applied.
- Before sending anything at all.

## Steps

1. **Load the open conditions** from the previous run. Not your memory of them
   — the rows.
2. **Pair each with this run's reading.** Same condition, same thing, same
   namespace.
3. **Classify each pair** against the table below.
4. **Write the rows first, send second.** A finding sent from a state that was
   never recorded cannot be compared against next hour, and the run after it
   reports the same thing again.
5. **Close what is gone.** A condition absent from this run's reading has
   cleared, and a clear that goes unsaid leaves whoever read the 🔴 still
   waiting.

## The table

| Previously | Now | Posted |
| --- | --- | --- |
| absent | present | 🔴 new |
| present | present, worse | 🟠 degraded |
| present | present, same | nothing |
| present | absent | 🟢 cleared |

## Two things this rule is not

**It is not silence.** Every run — including the quiet ones — records what it
saw. A monitor whose quiet hours leave no trace cannot be audited after an
incident, and "it was fine at 2am" has to be checkable rather than assumed.

**"Worse" is a number.** Four restarts to eleven is worse. Four restarts to
five, an hour later, is the same condition still running. If you cannot say
what got worse with two numbers, nothing got worse.

## Output

An updated `conditions` ledger, and a list — often empty — of what to report.
An empty list is a successful run.
