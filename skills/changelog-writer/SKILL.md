---
name: Changelog Writer
description: Turn a set of merged changes into a clear, user-facing changelog entry.
category: Content
---

# Changelog Writer

Convert a release's raw changes into a changelog that tells users what improved
and why they should care.

## When to use

- A release is about to ship and needs release notes.
- You are catching up an out-of-date changelog before an announcement.

## Steps

1. **Gather** the merged changes since the last release.
2. **Group** them into Added, Improved, Fixed — drop internal-only churn.
3. **Rewrite** each line in user terms: the benefit, not the commit message.
4. **Highlight** the one or two changes users will care about most.
5. **Add** the version, date, and any upgrade or breaking-change note.

## Output

A dated changelog entry grouped by Added / Improved / Fixed, written for users,
with breaking changes called out clearly.
