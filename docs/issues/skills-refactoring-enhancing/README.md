# Skills: scoping, scanning, threat model and authoring parity — design + rollout plan

Tracking issue: #2427. MCP context: #2373 (tiered tool-call permissions).

This is an **implementation brief**, not a permanent architecture doc — it lives
under `docs/issues/` rather than `docs/modules/` on purpose. It describes work
not yet done. Once the work lands, fold the relevant parts into the new
`docs/modules/skills.md` (see [`03-prerequisites.md`](03-prerequisites.md)) and
delete this directory.

Research date: 2026-09-21. Every `file:line` below was re-read against the tree
at `upstream/main` `acaefbc63` (2026-09-21). Line numbers drift; treat them as a
pointer to a symbol, not a promise.

**Who this is for:** an engineer or a fresh session with no prior context. Read
in order. [`01-current-state.md`](01-current-state.md) says what exists;
[`08-rollout.md`](08-rollout.md) says what to build first.

## The problem, briefly

Skills are `SKILL.md` documents an operator installs so agents can read them.
They are the one subsystem of comparable size to MCP with no owning doc, no
threat-model coverage, and none of the operator controls the rest of the
industry now treats as baseline:

1. a skill reaches **every agent**, company-wide — no per-agent or per-desk scope;
2. a registry install is snapshotted but **never scanned**, and skill
   descriptions and the catalogue reach the prompt verbatim;
3. skills are **absent from the threat model** (zero mentions in
   `agent-isolation.md`, `grants.md`, `approvals.md`, `tools.md`);
4. the stored `version` is **never compared**, so an install silently goes stale;
5. "skill" means two unrelated things (`SKILL.md` bundles vs `[place].skills`);
6. there is **no owning doc**;
7. authoring is thinner than the baseline (no upload, no bundled files, no
   AI-assisted create).

Details and citations: [`01-current-state.md`](01-current-state.md).

## The verdict: the MCP tier model does not transfer

#2373 replaces a flat allowlist with per-tool risk tiers because MCP tools are
live actions that need calibrated approval friction. A skill today confers **no
capability**: agents get three read tools (`list_skills`, `describe_skill`,
`read_skill_resource`), all classified `EffectGroup::Other, Reach::Nothing`
(`policy/consequence.rs:667-669`), and skill *execution* is deliberately not
wired (`harness/built_in/skills.rs:22-25`). If a skill's text persuades an agent
to call a tool, that call already crosses the normal
`[tools].allow ∩ desk.tools ∩ agent.tools` chain and `ApprovalGate`.

So there is nothing to tier. **Do not design a skill permission-tier model
before execution exists** ([`07-execution-deferred.md`](07-execution-deferred.md)).
The work that *is* worth doing is scoping, scanning, drift, docs and authoring.

Installing a skill is correctly **not** an `ApprovalGate` checkpoint: it is an
admin configuration action (`AdminScopedCompany`), the same category as adding a
teammate or an MCP server, not a mid-cycle agent effect.

## Decided vs open

**Decided (in this brief):**

- Keep skills read-only; keep execution deferred and gated on the seam in `07`.
- Per-agent scoping is **enforced at materialization**, not only hidden from the
  catalogue (`04`).
- Scan and sanitize on **install and on create/upload**, including description
  and catalogue text (`05`).
- Skills get a threat-model section and an owning doc first (`03`).

**Open (a human decides — see `08-rollout.md`):** scan strictness and whether it
blocks or warns; a manifest field vs an overlay side-table for scoping; whether
Discover ever shows popularity; hosted-mode registry fallback.

## The whole flow, today

Every skill takes the same path from a source to an agent's prompt. Note the last
box: nothing here can *run* a skill.

```text
 SOURCES
 ┌────────────┐ ┌────────────┐ ┌────────────┐ ┌────────────┐
 │ _globals/  │ │ company    │ │ shared     │ │ console    │
 │ skills/    │ │ bundle     │ │ registry   │ │ authored   │
 │ (embedded) │ │ skills/    │ │ library    │ │ custom     │
 └─────┬──────┘ └─────┬──────┘ └─────┬──────┘ └─────┬──────┘
       │ read-only    │ read-only    │ install      │ create
       │              │              │              │
       │              │              └──────┬───────┘
       │              │                     ▼
       │              │     ┌───────────────────────────────┐
       │              │     │ ADMIN ROUTES                  │
       │              │     │ install/create/toggle/        │
       │              │     │ uninstall                     │
       │              │     │ AdminScopedCompany            │
       │              │     │ per-company write_lock        │
       │              │     └───────────────┬───────────────┘
       │              │                     │
       │              │                     ▼
       │              │     ┌───────────────────────────────┐
       │              │     │ SkillStateStore               │
       │              │     │ deltas keyed (company, slug)  │
       │              │     └───────────────┬───────────────┘
       │              │                     │
       ▼              ▼                     ▼
 ┌──────────────────────────────────────────────────────────┐
 │ skill_effective::resolve                                 │
 │ fold: globals < bundle < deltas ; [globals].disable wins │
 └───────────┬────────────────┬───────────────┬─────────────┘
             ▼                ▼               ▼
      GET …/skills     GraphQL                │ HarnessPool::ensure
      (console list)   Company.skills         │ each cycle, on delta change
                                              ▼
                             ┌─────────────────────────────────┐
                             │ per-agent materialize           │
                             │ skill-catalog/, rebuilt per call│
                             │ ALL enabled skills -> EVERY     │
                             │ agent (no scoping today)        │
                             └────────────────┬────────────────┘
                                              │
                                              ▼
                             ┌─────────────────────────────────┐
                             │ prompt catalogue (read-only)    │
                             │ + list_skills / describe_skill /│
                             │   read_skill_resource tools     │
                             └────────────────┬────────────────┘
                                              │
                                              ▼
                                         agent turn
                                              │ any real action
                                              ▼
                        ┌──────────────────────────────────────────┐
                        │ [tools].allow ∩ desk.tools ∩ agent.tools │
                        └─────────────────────┬────────────────────┘
                                              │
                                              ▼
                                     ┌────────────────┐
                                     │ ApprovalGate   │
                                     └────────────────┘

 Not wired (which is why a skill stays passive text):
 ┌ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─┐
 ┆ run_workflow  --  NOT WIRED (no skill can execute)       ┆
 ┆ orchestrator-only upstream; seam missing (see 07)        ┆
 └ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─┘
```

- Routes and lock: `server/ops/skills.rs:101-108` (router), `:87` (`write_lock`),
  install `:307`, create `:439`, toggle `:404`, uninstall `:377`.
- Deltas and fold: `SkillStateStore` `ports/skills_state.rs:47`; `resolve`
  `company/skill_effective.rs:132`; `[globals].disable` delta `:92`.
- Consumers: `list_skills` `server/ops/skills.rs:265`, GraphQL
  `server/graphql/skills.rs`, `HarnessPool::ensure` `harness/built_in/mod.rs:3151`.
- Agent surface: `materialize` `harness/built_in/skills.rs:70` (called from
  `build.rs:1083`), catalogue `:169`, read tools `:148`, classified
  `Reach::Nothing` at `policy/consequence.rs:667-669`.
- Any real action uses the normal grant chain (`docs/spec/runtime/tools.md`) and
  `ApprovalGate` (`ports/approvals.rs:13`). `run_workflow` is not wired:
  `skills.rs:22-25`.

## The whole flow, target

The same spine with the additions marked `[NEW]`. Unmarked boxes are unchanged from
today. Nothing marked `[NEW]` exists yet.

```text
 SOURCES: _globals · bundle · registry · custom · upload [NEW]
                           │ install / create / upload
                           ▼
    ┌────────────────────────────────────────────┐
    │ [NEW] SCAN + SANITISE GATE                 │
 ┌─►│ body, description, category, files         │
 │  │ size caps · Unicode-tag strip              │
 │  │ verdict pass/warn/block: OPEN DECISION     │
 │  └──────────────────────┬─────────────────────┘
 │                         │ pass / warn only
 │                         ▼
 │  ┌────────────────────────────────────────────┐   ┌─────────────────────────┐
 │  │ ADMIN ROUTES  AdminScopedCompany           │   │ [NEW] AUDIT EVENT       │
 │  │ per-company write_lock                     ├──►│ install / update / scope│
 │  │ install · create · toggle · uninstall      │   │ scan verdict; digest,   │
 │  │ [NEW] upload · scope · update              │   │ actor. NO body ->       │
 │  └──────────────────────┬─────────────────────┘   │ journal                 │
 │                         │                         └─────────────────────────┘
 │                         ▼
 │  ┌────────────────────────────────────────────┐   ┌─────────────────────────┐
 │  │ SkillStateStore (deltas per slug)          │   │ [NEW] DRIFT CHECK       │
 │  │ [NEW] trust tier + pinned digest           ├──►│ on GET …/skills:        │
 │  │ [NEW] version, installer, time             │   │ digest+version vs       │
 └──┤ [NEW] per-agent allowlist                  │   │ live registry ->        │
    │       (absent = ALL agents)                │   │ updateAvailable or      │
    └──────────────────────┬─────────────────────┘   │ modified (skips edits)  │
                           │                         └─────────────────────────┘
                           ▼
    ┌────────────────────────────────────────────┐
    │ skill_effective::resolve                   │
    │ fold + [globals].disable (unchanged)       │
    └──────────────────────┬─────────────────────┘
                           │ GET …/skills [NEW: tier·scope·update]
                           ▼
    ┌────────────────────────────────────────────┐
    │ per-agent materialize                      │
    │ [NEW] apply allowlist FIRST: write only    │
    │ the intersection to skill-catalog/         │
    │ (no allowlist = every enabled skill)       │
    └──────────────────────┬─────────────────────┘
                           │
                           ▼
    ┌────────────────────────────────────────────┐
    │ catalogue + list/describe/read tools       │
    │ derived ONLY from what was materialized    │
    └──────────────────────┬─────────────────────┘
                           │
                           ▼
                      agent turn
                           │ any real action
                           ▼
    ┌────────────────────────────────────────────┐
    │ [tools].allow ∩ desk.tools ∩ agent.tools   │
    │ then ApprovalGate  (unchanged)             │
    └────────────────────────────────────────────┘

    ┌ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─┐
    ┆ [deferred] run_workflow  --  NOT WIRED     ┆
    ┆ stays gated behind the upstream seam (07)  ┆
    └ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─┘
```

- Gate, sanitise and verdict: [`05`](05-registry-trust-and-updates.md) §5.2-5.3; how
  strict the verdict is remains an OPEN DECISION in [`08`](08-rollout.md).
- Trust tier, pinned digest, provenance: `05` §5.4; drift and `update`: §5.5;
  audit event (no body) to the journal: §5.6.
- Scope: [`04`](04-per-agent-scoping.md). Absent allowlist means all agents, so no
  existing company changes behaviour; it is applied before `materialize`
  (`build.rs:1083`).
- The left-hand loop is the operator running `update`, which re-enters the gate.
- The dashed box stays dashed: execution is gated behind the upstream seam
  ([`07`](07-execution-deferred.md)).

## What this looks like when it ships

Illustrative only — not a component spec and not pixel-accurate.

**The Skills page** (today's `SkillsView.tsx` shape, with the new pieces marked):

```
┌────────────────────────────────────────────────────────────────────────┐
│  Connections › Skills                                                  │
│  Skills are reference material your agents read — not buttons they     │
│  press.                                                                │
├────────────────────────────────────────────────────────────────────────┤
│  [ Installed ]  Registry     [Search…]  [Filter ▾] [Sort: edited ▾]    │  ← Filter/Sort new
│                                                          [ + Add ▾ ]   │
│  ──────────────────────────────────────────────────────────────────    │
│  Company · 2                                                           │
│   ▣ brand-voice     Company    all agents               edited 3d  ⋮   │  ← source, scope, date new
│   ▣ weekly-report   Custom     @copywriter @editor      edited 5d  ⋮   │
│  Registry · 1                                                          │
│   ▣ meeting-brief   Registry   v1.2 · update available  all agents ⋮   │  ← drift signal new
└────────────────────────────────────────────────────────────────────────┘
```

**The Add menu** (`06-authoring-ux.md`):

```
                                                          [ + Add ▾ ]
                                         ┌──────────────────────────────┐
                                         │ ⬆  Upload skill              │
                                         │ ✎  Create a skill            │
                                         │ ✦  Draft with a teammate     │
                                         └──────────────────────────────┘
```

**Upload, with a scan verdict** (`05-registry-trust-and-updates.md`). The host
scans before it writes; the dialog shows the verdict (a dry run on pick, re-run on
Save), and a blocked result never reaches `SkillStateStore`:

```
┌──────────────────────────────────────────────────────────────┐
│  Upload skill                                             ✕  │
├──────────────────────────────────────────────────────────────┤
│  ┌ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ┐  │
│  │   Drop .md, .zip or .skill here — several at once     │  │
│  └ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ┘  │
│  • .md needs name + description in YAML frontmatter          │
│  • .zip / .skill must contain SKILL.md                       │
│                                                              │
│  Security scan          ✓ passed   ⚠ 1 warning   ✕ blocked   │
│    ⚠ description contains an instruction to ignore prior     │
│      instructions (line 3)                                   │
│                                        [ Cancel ]  [ Save ]  │
└──────────────────────────────────────────────────────────────┘
```

**Create a skill, with bundled files** (`06-authoring-ux.md`):

```
┌──────────────────────────────────────────────────────────────┐
│  Create a skill                                           ✕  │
├──────────────────────────────────────────────────────────────┤
│  Skill name    weekly-status-report                          │
│  Description   Generate weekly status reports from recent    │
│                work. Use when asked for updates.             │
│                (say what it does AND when to use it)         │
│  ┌────────────────────────────────────────────  + Add file ┐ │
│  │ 1  Summarize my recent work in three sections: wins,    │ │
│  │    blockers, and next steps…                            │ │
│  └─────────────────────────────────────────────────────────┘ │
│  [ Draft ]                              [ Cancel ]  [ Create ]│
└──────────────────────────────────────────────────────────────┘
```

**Per-agent scope** on a skill's detail panel (`04-per-agent-scoping.md`):

```
┌──────────────────────────────────────────────────────────────┐
│  meeting-brief  · Registry · v1.2                            │
├──────────────────────────────────────────────────────────────┤
│  Available to                                                │
│   ( ) All agents                                             │
│   (•) Selected agents                                        │
│        [x] copywriter   [x] editor   [ ] strategist          │
│  Agents that are not selected cannot list, describe or read  │
│  this skill's files.                                         │
└──────────────────────────────────────────────────────────────┘
```

## The UI flow, screen to screen

The mockups above are single screens; this is how an operator moves between them.
`today` is what `frontend/src/views/SkillsView.tsx` does now; `NEW Pn` is the phase
in [`08-rollout.md`](08-rollout.md) where the piece lands. The list is cards today,
not rows.

```text
CONNECTIONS > SKILLS   #/connections/skills                today
│
├─ tabs: Installed (cards) | Registry                      today (:86)
├─ Filter / Sort / last edited                             NEW P4
│
├─ Registry tab ─► browse library ─► [Install]             today
│      └─► SCAN ─► card appears                            SCAN NEW P1
│
├─ [Add skill] today  →  [+ Add ▾] menu                    menu NEW P4
│    ├─ Upload skill ─► pick .md/.zip/.skill ─► SCAN       NEW P4
│    │     ├─ pass  ─► [Save] ─► card appears
│    │     ├─ warn  ─► findings shown ─► Save allowed?     OPEN DECISION
│    │     └─ block ─► stays in dialog, Save disabled;
│    │              nothing reaches SkillStateStore
│    ├─ Create a skill ─► name/category/description/body   today
│    │     ├─ [Create] ─► SCAN ─► card appears             SCAN NEW P1
│    │     └─ + Add file                                   NEW P4, 06 §6.3
│    └─ Draft with a teammate ─► teammate drafts           NEW P4
│          ─► reviewed in the Create form ─► [Create]
│
├─ card controls
│    ├─ Enable switch                                      today (:415)
│    ├─ Uninstall (trash): Registry/Custom only            today (:431)
│    └─ ⋮ menu: Edit(custom) | Enable/Disable |            NEW P2-P4
│         Scope… | Update | Uninstall
│
├─ card click ─► Detail panel                              NEW P2
│      └─ Available to: All | Selected agents ─► [Save]
│
└─ "update available" badge ─► review vs pinned            NEW P3
       ├─ [Update] ─► SCAN ─► card refreshed
       └─ [Keep]  ─► pinned copy stays
   (an install edited locally is "modified": the
    check skips it and Update refuses)
```

- Registry install → scan: [`05`](05-registry-trust-and-updates.md) §5.2 (scan,
  P1); the install route exists (`server/ops/skills.rs:307`).
- Upload and its verdict: [`06`](06-authoring-ux.md) §6.2. Warn versus block is
  OPEN DECISION 1 in `08`. Showing the verdict before Save needs a no-write scan
  on the host; §6.2 does not specify one yet (see 06 §6.10).
- Create and Add file: `06` §6.3 — first slice is `SKILL.md` only, so Add file
  waits on that decision. Draft with a teammate: `06` §6.4, shown only when
  `designsProfiles` is true.
- Card controls: switch `SkillsView.tsx:415`; Uninstall `:431-437`, hidden for
  company skills (`server/ops/skills.rs:391-401`). The menu is `06` §6.6 and has no
  "Details" entry: a card click opens the Detail panel, where scope lives.
- Detail panel and scope: [`04`](04-per-agent-scoping.md) (console picker), P2.
  Update badge, review, `modified` skip: `05` §5.5, P3.

## Files

| File | Holds |
| --- | --- |
| [`01-current-state.md`](01-current-state.md) | How skills work today, route by route, and the seven gaps, with `file:line` |
| [`02-industry-comparison.md`](02-industry-comparison.md) | Claude, Codex, Gemini CLI, Cursor, OpenClaw, Hermes, OpenHands, Goose — with verified/inferred tags and sources |
| [`03-prerequisites.md`](03-prerequisites.md) | Owning doc, threat-model section, naming signpost, spec-conformant validation |
| [`04-per-agent-scoping.md`](04-per-agent-scoping.md) | Allowlist model, OpenHuman wiring, enforcement point, migration |
| [`05-registry-trust-and-updates.md`](05-registry-trust-and-updates.md) | Scan on install/save, trust tiers, pinning, drift check, provenance |
| [`06-authoring-ux.md`](06-authoring-ux.md) | Upload, bundled files, AI-assisted create, list UX, placement |
| [`07-execution-deferred.md`](07-execution-deferred.md) | What the `run_workflow` seam must provide before execution ships |
| [`08-rollout.md`](08-rollout.md) | Phases by blast radius, acceptance, tests, open decisions |
