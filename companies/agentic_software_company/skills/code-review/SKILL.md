---
name: Code Review
description: Review a change for correctness, safety, and clarity, and return actionable feedback.
category: Ops
---

# Code Review

Read a proposed change the way a careful teammate would and return feedback the
author can act on.

## When to use

- A change is up for review before it merges.
- A risky area needs a second set of eyes.

## Steps

1. **Understand** the intent from the description and the spec.
2. **Check** correctness — logic, edge cases, and error handling.
3. **Check** safety — inputs, auth, secrets, and data exposure.
4. **Check** clarity — naming, structure, and tests that prove it works.
5. **Write** feedback ranked blocking / should-fix / nit, each specific.

## Output

Review comments ranked blocking / should-fix / nit, each pointing at a line and
a fix, per the [[Engineering standards]]. A human approves the merge.
