# 06 — Authoring and list UX

**Goal.** Bring the console's Skills page up to the baseline an operator now
expects, without changing what a skill *is*. Everything here is presentation and
authoring on top of the existing routes, plus one new read-only drafting route.

Reference flow: Claude's Settings › Skills, described from screenshots in
[`02`](02-industry-comparison.md). It is a reference for shape, not a spec to
copy: OpenCompany's skills are company-wide admin objects, not per-user files.

## 6.1 Where things stand

`frontend/src/views/SkillsView.tsx` (621 lines): tabs "Installed" and "Registry"
(`:86-87`), a switch per skill, and `AddSkillDialog` (`:503-621`) with four fields —
name, category (a fixed `SkillCategory` list, default `"Marketing"`), one-line
description, and a Markdown "Playbook" body — posting to `POST …/skills`
(`frontend/src/api/skills.ts:113-119`). Copy-layer honesty already exists: the
read-only banner and "Agents can read this" (`frontend/src/lib/skills.ts:45`,
`:56`).

| Claude offers | OpenCompany today | Slice |
| --- | --- | --- |
| Upload `.md` / `.zip` / `.skill` | none | 6.2 |
| "+ Add file" (bundled resources) | one body only | 6.3 |
| "Create with Claude" | none | 6.4 |
| Description hint: what it does **and** when to use it | one-line field, no guidance | 6.5 |
| Filter, Sort, last-edited | search only | 6.6 |
| Source label ("from Anthropic") | shown as a plain lowercase word on each card (`SkillsView.tsx:424`); no tier, version or "from" | 6.6 |
| Row ⋮ menu | an Enable switch (`:415`) and, for non-company skills, an Uninstall icon button (`:431-437`); no menu | 6.6 |
| Skills / Connectors / Plugins as siblings | Skills under Connections | 6.7 |

## 6.2 Upload

`POST …/skills/upload` (multipart), admin-only like every skill write
(`AdminScopedCompany`, `server/ops/skills.rs:439`). Accept:

- **`.md`** — must carry `name` and `description` in frontmatter (Claude's rule,
  verified); becomes the document body;
- **`.zip` / `.skill`** — must contain `SKILL.md` at the root or in a single top
  directory; every other file is a bundled resource.

Several files at once, one result row each. The same validator and scanner as
[`03`](03-prerequisites.md) §3.4 and [`05`](05-registry-trust-and-updates.md) §5.2
run **before** anything is persisted; a `block` verdict returns the report and
writes nothing. Archive handling is a real attack surface — reject path
traversal, absolute paths, symlinks, nested archives, and enforce an
uncompressed-size cap and a file-count cap before extracting (zip bombs). The
stored form stays what it is today: the assembled `SKILL.md` as `custom_doc`
(`ports/skills_state.rs:31`) — see 6.3 for where resources go.

**Open point (6.3):** `SkillState.custom_doc` is a single `Option<String>`. It
cannot hold bundled resource files. Upload of a `.zip` therefore either drops the
extras (unacceptable — silently losing files is the failure mode this repo keeps
writing tests against) or forces a storage decision.

## 6.3 Bundled files: the one real design question in this file

The materializer already writes "each enabled skill's `SKILL.md` (+ bundled
resource files)" (`harness/built_in/skills.rs:70-127`) — for **bundle** skills
that live on disk. A console-authored or uploaded skill has only `custom_doc`.

| Option | Shape | Notes |
| --- | --- | --- |
| **A. Resources in the delta** | `SkillState.resources: Vec<{path, content}>` beside `custom_doc`, size-capped | simplest; rides the existing port and export/import; bounded by a total cap; text-only unless base64 |
| **B. Workspace-backed** | store under a reserved workspace root (`docs/spec/runtime/ports-console-workspace.md`) and reference by path | reuses the note tree and its binary half; more moving parts; a new reserved root name (`workspace-names.md` rules) |
| **C. Defer** | create/upload accepts `SKILL.md` only; "+ Add file" waits | ships 6.2/6.4/6.5/6.6 now; honest, and matches today |

**Recommendation: C for the first slice, A when a real skill needs a script or
reference file.** It keeps the per-file scan surface small while the scanner is new.
Whichever is chosen, extend `SkillState`; do not add a parallel store. Decide before building upload, because
6.2 depends on it.

## 6.4 Draft with a teammate

Claude's "Create with Claude" has a direct precedent here. The teammate copilot
already drafts a persona from a conversation: `POST …/team/{agentId}/draft` and
`POST …/team/draft`, body `{messages}`, answer `{reply, text?}`, **neither route
writes** and the host stores no transcript
(`docs/spec/runtime/api-team-drafting.md:8-40`). Workflows have
`POST …/workflows/draft-from-description`
(`docs/modules/server/workflow-authoring-routes.md:32`).

Add `POST …/skills/draft` with the **same contract**: the console owns the
transcript, the response is text the operator may take and save through
`POST …/skills`, nothing is written. It needs a model, so it inherits the existing
availability rule: `profile_drafter()` (`server/ops/team_agent.rs:1910`) is absent on
a `sidecar` or `custom` cognition path, and `GET …/inference` reports
`designsProfiles` so the dialog decides its own affordance
(`api-team-drafting.md:141`). Hide the button when it is `false`; do not render a
control that answers 5xx.

Prompt it with the spec's authoring guidance (description states **what it does
and when to use it**; keep the body short; put detail in resources) and run the
result through the same scan before showing it, so the assistant cannot hand the
operator a document the save path would refuse.

## 6.5 Description guidance

A placeholder and a one-line hint under the field: *"Say what it does and when
an agent should use it — e.g. 'Generate weekly status reports from recent work. Use
when asked for updates.'"* The description is what sits in every agent's catalogue
and what the model uses to decide relevance
([`02`](02-industry-comparison.md): progressive disclosure), so a vague one makes a
skill inert. Show a live character count against the 1024 limit from
[`03`](03-prerequisites.md) §3.4.

## 6.6 The list

- **Source/tier label** on every row from [`05`](05-registry-trust-and-updates.md)
  §5.4 ("Company", "Registry v1.2", "Custom"), and "update available" from §5.5.
- **Scope** from [`04`](04-per-agent-scoping.md): "All agents" / "3 agents" / "No
  agents".
- **Last edited** — needs a timestamp on `SkillState`; today it stores none. Add it
  with the same PR as the audit event so the two cannot disagree.
- **Filter** (source, tier, enabled, has-update) and **Sort** (last edited, name).
  Client-side is fine: the list is one company's skills, not a catalogue.
- **Row ⋮ menu:** Edit (custom only), Disable, Scope…, Update, Uninstall. Reuse the
  existing rule that a `Company`-bundle skill can only be disabled
  (`server/ops/skills.rs:391-401`) by greying Uninstall with a reason.
- **Layout at the real cap.** Validate the list with a realistic worst case — the
  global baseline plus a full registry install plus custom skills — not three demo
  rows, and in light and dark; a layout that works with three rows can fail at the cap.

## 6.7 Placement

Skills sits under Connections because installing one is "the same act as connecting
an app" (`frontend/src/views/settings-pages.ts:89`); the Connections section applies
that test to Inference, Hosting, Search and MCP servers too. Claude puts Skills,
Connectors and Plugins as siblings under "Customize", which is the same grouping
logic. **No move is proposed.** If a plugin-like bundle concept (a set of skills
plus connections installed together) is ever introduced, revisit the section's
information architecture then, in `docs/spec/runtime/ledgers-console-ia.md` and
`console-sections.md` (Rule 6, Rule 8).

## 6.8 Registry tab

Keep "Registry" and its metadata-only cards. Do **not** add the install-count
badges Claude's Discover cards show, for the reason in
[`05`](05-registry-trust-and-updates.md) §5.7; if a count is wanted later it is a
server-computed, per-host number. A "For you" ranking is out of scope. A skill card
should show its tier, version, description, and — because installing is the trust
decision — the scan verdict of the library copy.

## 6.9 Tests and verification

- `frontend/` unit tests for the dialog states (empty, invalid frontmatter, scan
  `warn`, scan `block`, upload with two files, one failing).
- Run **all three** typecheck gates (`npm run typecheck`, `typecheck:unit`,
  `typecheck:e2e`, `frontend/package.json:16-18`) — they use different tsconfig
  files, so a file clean under one can fail another.
- Playwright: create, upload, draft, filter, uninstall, at 1280 and 390 wide, light
  and dark, with screenshots. `scripts/ci/assert-design-tokens.sh` rejects raw hex.
- Host: multipart size limits, malicious archive fixtures (traversal, symlink,
  bomb), and that a `block` upload leaves `SkillStateStore` untouched.

## 6.10 The authoring flow, screen to screen

The Add menu in detail. `today` is `SkillsView.tsx` now (one "Add skill" button,
`:261`, opening the dialog at `:503`); `NEW Pn` is the phase in
[`08`](08-rollout.md).

```text
[+ Add ▾]   (today: one "Add skill" button, no menu)       NEW P4
│
├─ Upload skill                                            NEW P4
│    ▼
│  dialog: drop .md / .zip / .skill  (several at once)
│    ▼   per file: validate (03 §3.4)
│    │      └─ fail ─► inline error, file dropped
│    ▼   SCAN as a dry run ─► verdict shown in dialog      NEW P1
│    ├─ pass  ─► [Save] enabled
│    ├─ warn  ─► findings listed ─► [Save] enabled?        OPEN DECISION
│    └─ block ─► [Save] disabled; nothing is written
│    ▼   [Save] ─► host re-runs SCAN before it writes
│    ▼   SkillStateStore.set ─► card appears
│
├─ Create a skill                                          today
│    ▼   form: name · category · description · body
│    │   description hint: what it does AND when           NEW P4
│    │   counter vs the 1024 cap (the cap itself: P0b)     NEW P4
│    ├─ + Add file ─► resource attached to the form        NEW, 06 §6.3
│    ▼   [Create] ─► validate ─► SCAN                      SCAN NEW P1
│    ▼   pass / warn / block exactly as above ─► card
│
└─ Draft with a teammate                                   NEW P4
     shown only when GET …/inference says
     designsProfiles is true (else the button is hidden)
     ▼   conversation: console keeps the transcript,
     │   the host stores nothing
     ▼   POST …/skills/draft ─► {reply, text?}
     ▼   draft text is SCANNED before it is shown          NEW P1
     ▼   operator takes it ─► Create form, pre-filled
     ▼   [Create] ─► same path as "Create a skill"
```

- Upload: §6.2. Validation is [`03`](03-prerequisites.md) §3.4; the scan is
  [`05`](05-registry-trust-and-updates.md) §5.2. Warn versus block is OPEN DECISION 1
  in `08`.
- **A gap this diagram exposes.** The verdict is shown before Save, which needs a
  scan that reports without persisting (for example a `dryRun` flag on the upload
  route). §6.2 only says a `block` returns the report and writes nothing. Decide the
  dry-run shape with P4; Save must still re-run the scan on the host, since the
  client's earlier verdict is not trusted.
- Create: today's form has name, category, description and body. Description
  guidance and the counter are §6.5; the 1024 cap itself lands in P0b.
- Add file: §6.3 — deferred until the bundled-files storage decision.
- Draft: §6.4 — the route contract is `POST …/team/draft`'s (`docs/spec/runtime/api-team-drafting.md`),
  and the draft is scanned before it is shown so the assistant cannot hand over a
  document the save path would refuse.
