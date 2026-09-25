# 08 — Rollout plan

Phases are ordered by **blast radius**, smallest first, and each is one or more
PRs that a reviewer can hold in their head. Each phase becomes its own issue
(#2427 tracks the design only). Nothing in P0–P4 wires skill execution.

Legend: **Radius** = what breaks if it is wrong. **Reversible** = can it be reverted
without data migration.

## Summary

| Phase | Ships | Code? | Radius | Reversible | Depends on |
| --- | --- | --- | --- | --- | --- |
| **P0** | Owning doc, threat-model section, naming signpost, glossary | docs only | none | yes | — |
| **P0b** | Spec-conformant validation (slug length, description cap) | small | a manifest or skill that was accepted may now be rejected | yes | P0 (defines "correct") |
| **P1** | Scan + prompt sanitisation, on install and create | yes | installs/creates that used to succeed may `warn`/`block` | yes (flag) | P0b (one validator) |
| **P2** | Per-agent skill scope | yes | which agents see which skills; manifest schema | yes (absent = all) | P0 |
| **P3** | Provenance, digest, drift signal, `update`, audit event | yes | new stored fields; new journal event | mostly (fields additive) | P1 |
| **P4** | Authoring UX: upload, draft, list UX (bundled files gated on a decision) | yes | console only, plus one upload route | yes | P1, P3 |
| **P5** | Execution | — | **deferred** | — | upstream seam ([`07`](07-execution-deferred.md)) |

P2 and P1 are independent and can run in parallel; P3 wants P1's scan record; P4's
list UX wants P2 and P3's fields. Every phase is shippable alone.

## P0 — Docs and vocabulary (no code)

**Scope.** [`03`](03-prerequisites.md) §3.1–3.3: `docs/modules/skills.md`; a skills
section in `docs/spec/security/agent-isolation.md` plus one-line pointers in
`grants.md`, `approvals.md`, `tools.md`; the two-meanings signpost and glossary
entries.

**Acceptance.**
- The four security/grant docs each mention skills, and none implies skills are
  sandboxed or that installing one is a privilege grant.
- `docs/modules/skills.md` ≤500 lines with re-verified citations; linked from
  `docs/spec/README.md`.
- Every changed Markdown file ≤500 lines.

**Verification.** `wc -l`; reviewer re-greps each `file:line`. No CI lanes beyond
the docs checks.

## P0b — Validation

**Scope.** [`03`](03-prerequisites.md) §3.4: slug ≤64, description ≤1024, frontmatter
block cap, one validator function shared by `create_custom`.

**Acceptance / tests.** In `company/skill_file_tests.rs`: 65-char slug rejected;
1025-char description rejected; newline-plus-fake-key still collapsed; an existing
bundle's skills all still parse (run against `companies/_globals/skills/`).
**Risk to check first:** every currently shipped skill must pass, or the phase
breaks a baseline. `cargo fmt --all -- --check`, `cargo clippy --all-targets -- -D
warnings`, `cargo test`.

## P1 — Scan and sanitize

**Scope.** [`05`](05-registry-trust-and-updates.md) §5.2–5.3: `scan_skill`, run on
registry install, custom create and the empty-registry fallback; catalogue
rendering as quoted data with invisible code points stripped.

**Acceptance.**
- A poisoned description-only fixture and a poisoned-resource-only fixture are each
  caught; benign near-misses pass.
- The catalogue for a description containing `\n\nSystem:` is a single quoted line.
- Existing behaviour preserved: unknown slug in a non-empty registry is `404`; a
  broken configured library is `500` (`server/ops/skills.rs:286-306`).
- Rollout behind a setting so the verdict can start at `warn` and be raised to
  `block` (see open decision 1).

**Verification.** Unit tests per check; a handler test that a `block` writes nothing
to `SkillStateStore`; the full Rust trio; one real registry install through the
console showing the verdict.

## P2 — Per-agent scope

**Scope.** [`04`](04-per-agent-scoping.md): `skills` on the agent (and its override),
`resolve_for_agent`, filtering in `harness/built_in/build.rs:1083` before
`materialize`, DTO/GraphQL exposure, console picker.

**Acceptance.**
- Absent = all (no existing company changes behaviour); `[]` = none; a list narrows;
  intersection with company enable/disable and `[globals].disable`.
- Agent B cannot `read_skill_resource` a skill scoped only to agent A (asserted).
- Editing one agent's scope rebuilds only that agent's tree and leaves other agents'
  conversation state alone.
- Export/import round-trips the field; a manifest with an unknown slug loads with a
  warning.

**Verification.** Rust trio; **all three** frontend typechecks (`npm run typecheck`,
`typecheck:unit`, `typecheck:e2e`); the picker seen running in a browser in light and
dark at its real cap (a company with the full baseline plus registry plus custom
skills), with screenshots. `manifest.md`/`agents.md` updated in the same PR
(`breaking` label check: it is additive, so no).

## P3 — Provenance, drift and audit

**Scope.** [`05`](05-registry-trust-and-updates.md) §5.4–5.6: tier label; digest,
version, installer and time recorded per install; `updateAvailable` on `GET
…/skills`; `POST …/skills/{slug}/update` with re-scan and diff; a journal event with
no body.

**Acceptance.** A library edit flips `updateAvailable`; editing an installed copy
flips it to `modified` and `update` refuses; the event carries digest and actor and
no document; `docs/spec/runtime/events.md` documents the new event.

**Verification.** Rust trio; the events doc stays ≤500 lines; export/import carries
the new fields and an older bundle without them imports cleanly (defaults).

## P4 — Authoring UX

**Scope.** [`06`](06-authoring-ux.md): upload route and dialog; `POST …/skills/draft`;
description guidance; Filter/Sort/last-edited/source label/row menu. Bundled files
only after the open decision below.

**Acceptance.** Every dialog state has a test; a hostile archive (traversal, symlink,
bomb) is rejected before extraction; `draft` is hidden when `designsProfiles` is
false; the list holds at the real cap in light and dark at 1280 and 390 wide.

**Verification.** Three frontend typechecks; unit tests; Playwright with screenshots
using the repo's pinned Playwright; `scripts/ci/assert-design-tokens.sh`.

## P5 — Execution (deferred)

Not scheduled. Gate: the upstream seam and the eight requirements in
[`07`](07-execution-deferred.md). Do not open an implementation issue until
OpenHuman has an injection path for config, metering and egress.

## Follow-up issue split

File one issue per phase, each linking #2427, labelled `cluster:skills`:

1. P0 — docs and vocabulary (`documentation`)
2. P0b — spec-conformant validation
3. P1 — scan and sanitize (security-relevant; likely `priority: p1` or `p2`)
4. P2 — per-agent scope
5. P3 — provenance, drift, audit
6. P4 — authoring UX (may split: upload / draft / list)
7. P5 — execution (`phase-2`, blocked on upstream)

## Open decisions (a human decides)

1. **Scan strictness.** Does P1 start at `warn` or `block`? Is `block` overridable by
   an admin (Hermes allows `--force` for non-dangerous findings only)? Recommendation:
   `warn` first, `block` for invisible-code-point and credential findings from day one,
   promote after a release of real use.
2. **Where per-agent scope lives.** A manifest field on `[[agent]]` with an override
   (option A) or a field on `SkillState` (option B). Recommendation A, per
   [`04`](04-per-agent-scoping.md); it is a manifest schema change, so it needs an
   explicit yes.
3. **Bundled files.** Ship SKILL.md-only authoring first (C), or store resources on
   `SkillState` (A), or use the workspace (B)? Blocks the upload of `.zip`
   ([`06`](06-authoring-ux.md) §6.3).
4. **Popularity in Discover.** Show install counts at all? If yes, computed
   server-side per host only. Recommendation: no, for now
   ([`05`](05-registry-trust-and-updates.md) §5.7).
5. **Hosted empty-registry fallback.** Keep letting an admin's request body author a
   "registry" install on hosts with no library? Recommendation: keep it, label it
   `custom`, and scan it like any other custom skill.
6. **`name` must equal slug.** The spec says yes; today `name` is a display string.
   Enforcing it may reject existing skills — a compatibility call.
7. **Per-desk scope.** A third operand in the intersection, deferred in
   [`04`](04-per-agent-scoping.md). Yes/no, and when.
8. **Signing and third-party registries.** Recorded as future work; needs a publisher
   and key story that does not exist.

## Confidence

The current-state claims in [`01`](01-current-state.md) were re-read against source on
2026-09-21. The ecosystem claims in [`02`](02-industry-comparison.md) carry their own
verified / inferred / snippet-only / unconfirmed tags; a `snippet-only` or
`unconfirmed` item should be re-checked before it is used to justify a decision.
