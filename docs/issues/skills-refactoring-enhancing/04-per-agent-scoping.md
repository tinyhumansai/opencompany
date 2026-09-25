# 04 — Per-agent skill scoping

**Goal.** An operator can say which agents may use which skills. Today every
enabled skill reaches every agent (`harness/built_in/skills.rs:18`), and the only
lever is disabling it for the whole company.

**Why it is worth doing even though skills are read-only.** A skill's text steers
an agent. A finance-only playbook in the copywriter's context is noise at best
and a leak at worst; a registry skill the operator trusts for one teammate should
not be handed to all of them. It is also the smallest change that closes a gap
the industry has already closed: OpenClaw has per-agent allowlists, Claude's SDK
has a per-session `skills` allowlist, Codex has per-skill implicit-invocation
policy ([`02`](02-industry-comparison.md)).

## What already exists

- **Upstream OpenHuman already supports it.** `RunWorkflowTool` carries
  `skill_allowlist: Option<HashSet<String>>` and `with_skill_allowlist`
  (`vendor/openhuman/crates/openhuman-core/src/agent/tools/run_workflow.rs:215`,
  `:246`, and a second copy at `:431`/`:458`), enforced at `:333`. Those paths are
  the checked-out submodule at `e9c23dcd7`; `upstream/main` records `3b029c85b`
  for `vendor/openhuman`, so re-check the lines after `git submodule update`.
- **We never pass it.** `grep -rn "skill_allowlist\|with_skill_allowlist"
  crates/opencompany-core/src` returns nothing (re-run 2026-09-21).
- **Its scope is execution, not reading.** That allowlist governs which skills
  `RunWorkflowTool` may *run*. Our three read tools (`WorkflowListTool`,
  `WorkflowDescribeTool`, `WorkflowReadResourceTool`) are separate types
  constructed at `harness/built_in/skills.rs:157-159`. Whether they can take an
  equivalent filter is **not established** here — check upstream before assuming
  the mechanism transfers. The design below does not depend on it.
- **The enforcement point is already per-agent.** `EffectiveSkills::materialize`
  (`skills.rs:70`) is called once per agent from
  `harness/built_in/build.rs:1083`, inside a scope that has `manifest_agent`
  (`build.rs:1076`), and it already builds a per-agent directory
  `<agent>/skill-catalog/`. Only the *inputs* are company-wide.

## Enforce at materialization, not only in the catalogue

Claude's Agent SDK hides an unlisted skill from the model **but leaves its files
readable through the Read and Bash tools** ([`02`](02-industry-comparison.md),
verified). If OpenCompany only trimmed the prompt catalogue, an agent could still
`read_skill_resource` a skill it "does not have". Because we materialize a
per-agent tree, the correct place is to **not write the skill to that agent's
`skill-catalog/` at all**: filter the enabled set by the agent's scope before
`materialize` writes, and the catalogue and the three read tools then agree by
construction, because they are derived from what was materialized
(`skills.rs:148-193`).

## Data model — two options

The precedent for a per-agent list with three states is `tools`. `Agent::tools`
is `Option<Vec<String>>` (`company/types.rs:773`): absent inherits the company's
standard grant, `[]` is an explicit deny-all since issue #1804, a list narrows.
`AgentOverride::tools` (`ports/types.rs`, the double-option beside
`AgentOverride` at `:3611`) lets an operator override the manifest line from the
console. `skills` should copy that contract exactly.

| Option | Shape | For | Against |
| --- | --- | --- | --- |
| **A. Agent-side field** | `[[agent]] skills = ["meeting-brief", "brand-*"]` on `company::Agent`, with a matching `AgentOverride::skills` double-option so `PATCH …/team/{agentId}` can edit it | one place to look ("what can this teammate use?"); reuses the `tools` three-state semantics and its console plumbing | a manifest schema change (`docs/spec/runtime/manifest.md`, `agents.md`); a per-agent edit surface to build |
| **B. Skill-side overlay** | a new field on `SkillState` (`ports/skills_state.rs:31`), e.g. `agents: Option<Vec<String>>`, edited by `PUT …/skills/{slug}` | no manifest change; the scope lives beside `enabled`, so the existing skill switch row grows one control; the existing `write_lock` already serialises it | "which skills does agent X have?" needs a scan of all skills; a `Company`-source skill has a delta only when overridden |

**Recommendation: A for the model, B's UI for the surface.** Store scope on the
agent (one source of truth, same semantics as `tools`), but render the picker on
the skill's detail panel as well as the teammate page, both writing the agent
field. That is the decision a human should confirm ([`08`](08-rollout.md) open
decisions), because it is a manifest field. If a manifest change is unwelcome,
B ships with no schema change and the same enforcement.

Glob support: `tools` accepts globs (`"docs.*"`, `agents.md:78`). Allow a trailing
`*` on slugs (`"brand-*"`) so a bundle of related skills is one entry; do not
invent a richer language.

## Semantics (state them in the doc, test each row)

| Agent's `skills` | Meaning |
| --- | --- |
| absent | inherits: every enabled skill, exactly today's behaviour |
| `[]` | **explicit none** — the agent gets no catalogue and no read tools |
| `["a","b"]` | only `a` and `b`, and only if they are also enabled company-wide |

- **Intersection, not union.** A skill disabled company-wide is off for everyone
  regardless of an agent's list, and a `[globals].disable = ["skill:…"]` still
  wins ([`01`](01-current-state.md)). Scope only ever narrows.
- **A non-empty list is final** (OpenClaw's rule, verified): it does not merge with
  defaults. That avoids the "why can this agent still see X?" class of bug.
- **An unknown slug in the list** is a validation warning at manifest load, not an
  error, so retiring a skill does not brick a manifest — but the console shows it.
- **No skill scoped to anyone** is legal (the skill is enabled and simply unused).

## How an allowlist is resolved

The flow the sections above describe, end to end. Boxes marked "proposed" do not
exist; `resolve` and `materialize` do.

```text
 ┌───────────────────────────────────┐   ┌────────────────────────────────────┐
 │ [[agent]] skills = [..]           │   │ AgentOverride::skills              │
 │ manifest field  (proposed)        │   │ PATCH …/team/{id} (proposed)       │
 └─────────────────┬─────────────────┘   │ beats the manifest line            │
                   │                     └──────────────────┬─────────────────┘
                   │                                        │
                   ┤────────────────────────────────────────┘
                   ▼
 ┌───────────────────────────────────┐   ┌────────────────────────────────────┐
 │ agent's allowlist                 │   │ effective set (company-wide)       │
 │ Option<Vec<slug or prefix*>>      │   │ resolve(): globals < bundle <      │
 └─────────────────┬─────────────────┘   │ deltas; [globals].disable wins     │
                   │                     └──────────────────┬─────────────────┘
                   └──────────────────┬─────────────────────┘
                                      │
                                      ▼
             ┌─────────────────────────────────────────────────┐
             │ INTERSECT  (scope only ever narrows)            │
             │ absent  -> ALL enabled skills (back-compat)     │
             │ []      -> none                                 │
             │ list    -> list  ∩  effective set               │
             └────────────────────────┬────────────────────────┘
                                      │
                                      ▼
             ┌─────────────────────────────────────────────────┐
             │ materialize(intersection ONLY)                  │
             │ an unlisted skill is never written to           │
             │ <agent>/skill-catalog/                          │
             └────────────────────────┬────────────────────────┘
                                      │
                                      ▼
             ┌─────────────────────────────────────────────────┐
             │ catalogue + list/describe/read tools            │
             │ derived from the materialized tree, so they     │
             │ cannot disagree with what is on disk            │
             └─────────────────────────────────────────────────┘

 ┌ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─┐
 ┆ NOTE: hiding a skill from the catalogue is NOT enough. Claude's SDK        ┆
 ┆ leaves an unlisted skill's files readable through Read/Bash. Here an       ┆
 ┆ unlisted skill is never written for that agent, so read_skill_resource     ┆
 ┆ has nothing to open.                                                       ┆
 └ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─┘
```

- `resolve` `company/skill_effective.rs:132`; `[globals].disable` delta `:92`.
- Same three-state contract as `tools`: `Agent::tools` `company/types.rs:773`,
  `AgentOverride` `ports/types.rs:3611`.
- `materialize` `harness/built_in/skills.rs:70`, called per agent from
  `build.rs:1083`; catalogue `:169`; read tools `:148`.
- Upstream `skill_allowlist` `vendor/openhuman/.../run_workflow.rs:215`, `:246` is
  the run-side twin and is not passed by OpenCompany today.

## Changes by layer

1. `company/skill_effective.rs` — add `resolve_for_agent(agent, …)` (or a
   `filter` on `EffectiveSkill`) that applies the list; keep `resolve` unchanged
   so the console still sees the whole set.
2. `harness/built_in/build.rs:1083` — pass the agent's scope into `materialize`;
   skip the whole block (as it already does when nothing is installed,
   `build.rs:1067`) when the filtered set is empty.
3. `company/types.rs` / `ports/types.rs` — the `skills` field and its override,
   with the same serde treatment as `tools`; a `manifest.rs` validator for
   unknown slugs and duplicate entries.
4. `server/ops/team*.rs` and `server/ops/skills.rs` — accept the field on
   `PATCH …/team/{agentId}` and expose an agent's scope on the skill DTO.
5. `server/graphql/skills.rs` — report per-skill `agents` so the console reads it.
6. Console — the picker in the README mockup; the row label from
   `skillReachLabel` (`frontend/src/lib/skills.ts:56`) becomes "All agents" /
   "3 agents" / "No agents".

## Migration and back-compat

None needed: an absent field means "all", which is what every existing company
has. Export/import (`store::export`) carries the field with the agent. Nothing is
removed, so the PR is not `breaking`.

## Tests

- `skill_effective_tests`: absent/`[]`/list against enabled, disabled, and
  globals-disabled skills; glob entry; unknown slug.
- `harness/built_in/skills_tests`: agent A's `skill-catalog/` contains the skill,
  agent B's does not, **and B's `read_skill_resource` cannot reach it** (the
  Claude-SDK leak, asserted directly).
- `HarnessPool::ensure` rebuild: editing an agent's list rebuilds that agent's
  tree on the next cycle and leaves the others' conversation state alone
  (`skills.rs:10-20`).
- A manifest with `skills = []` on one agent parses, round-trips through export
  and import, and shows "No agents" in the console.

## Out of scope

Per-**desk** scoping (a `desk.skills` middle level to match the three-level tool
grant). It is the obvious next step and the design leaves room for it — the
intersection rule above extends to a third operand — but nothing in the industry
survey requires it, and one level is enough to close the gap.
