# 07 — Skill execution: deferred, and what has to exist first

**Status: deferred on purpose. Nothing in this brief wires it.** This file records
the requirements so the day someone wants it, the bar is written down, and so that
nobody designs a skill permission model against a capability that does not exist.

## What is deferred

Skills are read-only today. `harness/built_in/skills.rs:22-25`: "skill *execution*
(`run_workflow`) is not wired here. `RunWorkflowTool` reaches for the global
`Config::load_or_init()` and bypasses the harness's metering, so it needs an
upstream injection seam that does not exist yet — it is out of scope for this
slice." Agents get `list_skills`, `describe_skill` and `read_skill_resource` only,
all `EffectGroup::Other, Reach::Nothing` (`policy/consequence.rs:667-669`). The
harness test `dispatched_belt_excludes_every_deferred_family`
(`harness/built_in/build_tests_part2.rs:569`) pins `run_skill`, `skill_run`,
`run_workflow` and `await_workflow` off every dispatched belt; only the
orchestrator holds `RunWorkflowTool` (described in
`frontend/src/lib/skills.ts:25-35`, issue #569).

## The rule

> **Do not design a permission-tier model for skills until execution exists.**

The MCP tier model (#2373) exists because MCP tools are live actions. A skill that
only supplies text has nothing to tier; the actions it might prompt are already
gated where they happen (the three-level grant and `ApprovalGate`). This changes
the day a skill can *run*: it stops being passive text and becomes an effect, and
it then needs exactly the consequence classification MCP tools get. Designing the
tiers first would be guessing at an interface. The tier-shaped work worth doing now
is the threat-model section in [`03`](03-prerequisites.md), which says this out loud.

## What the upstream seam does today

All from the checked-out `vendor/openhuman` submodule (commit `e9c23dcd7`;
`upstream/main` records `3b029c85b`, so re-verify after a submodule update):

- `RunWorkflowTool` spawns a fresh autonomous background run and waits inline (up to
  600 s per the research pass; not re-read here). Its
  `permission_level()` is `PermissionLevel::None` — the code comment justifies it as
  "no approval gate … the parent is already inside an autonomous context"
  (`crates/openhuman-core/src/agent/tools/run_workflow.rs:309-315`).
- The run path calls `Config::load_or_init()` directly
  (`skills/runtime/run_machinery.rs:98`, `:210`), and `resolve_workspace_dir()`
  does too, with a 30 s timeout (`skills/schemas/helpers.rs:46-47`). That is the
  "global config" the OpenCompany comment refers to.
- No metering or usage reference was found in `run_machinery.rs` (grep returned
  nothing during the research pass).
- **New finding:** when the loaded config's egress allow-list is empty, the run
  sets `http_request.allowed_domains = ["*"]`
  (`run_machinery.rs:226-227`), with a comment that it preserves any configured
  policy. A spawned skill run therefore gets **wildcard egress by default**.
- The guardrails are process-global spawn and nesting caps, not per-lineage budgets
  (module doc, `run_workflow.rs:1-30`).
- Upstream also ships a write family — `create_skill`,
  `install_workflow_from_url`, `uninstall_workflow` — in a default-off
  `workflow_manage` family, with an HTTPS installer (size cap, private-IP
  rejection) and a trust marker at `<workspace>/.openhuman/trust`
  (`skills/README.md`, from the research pass; not re-read here).

## Requirements before execution ships

1. **Injected configuration.** `spawn_workflow_run_background_with_profile`
   (`run_machinery.rs:69`) must take the harness's config, not load a global. This
   is an upstream change (PR to `vendor/openhuman`, per the repo's reuse-first rule
   in `docs/spec/integrations/README.md`), not a local patch.
2. **A metering/usage sink.** A skill run must be charged to the calling company
   and agent through the same `metering` path a normal turn uses
   (`crates/opencompany-core/src/metering/`), so `budget_usd_daily` still binds. Today
   a run "bypasses the harness's metering".
3. **A harness-supplied egress policy.** The harness passes an explicit allow-list;
   the `["*"]` fallback must never apply to a skill run. Default deny, declared per
   skill or per agent.
4. **A real approval gate on the run's own steps.** Two facts, both read from source
   on 2026-09-21, bound how big this gap is:
   - OpenCompany's policy layer is **already fail-closed by tool name.** Upstream's
     tool is named `run_workflow` (`vendor/openhuman/.../run_workflow.rs:41`), and
     that name is declared `Reach::Consequence` in `policy/consequence.rs:447`
     ("performs whatever that workflow performs, which this layer cannot see. It
     parks, and it stays a per-call decision"). A tool with a name nobody declared,
     and no read-only prefix, defaults to `Reach::Consequence`, `Standing::PerCall`
     and is never `Grantable` (`consequence.rs:2534-2562`). So *starting* a run
     parks under today's policy.
   - What is **not shown** is whether the spawned run's own tool calls cross
     `ApprovalGate`. Upstream justifies `permission_level() == None` because "the
     parent is already inside an autonomous context"
     (`run_workflow.rs:309-315`), and the run builds its own agent from a loaded
     config (`run_machinery.rs:210-`), not from the harness. Whether that agent is
     subject to OpenCompany's grant chain is **unverified** and must be established
     before wiring, not assumed.
5. **Consequence classification, explicitly.** Do not rely on the fail-closed
   default. Rename at the boundary as issue #845 did for the read tools
   (`harness/built_in/skills/naming.rs`) — upstream's `run_workflow` collides with
   OpenCompany's own company-workflow tool of the same name — and declare the new
   name in `policy/consequence.rs` with an `EffectGroup` and `Reach` that reflect
   what the skill's own steps can do, the way `mcp_call_reach` classifies MCP calls
   (`policy/consequence.rs:1942`). A coverage test already fails when a registered
   tool is undeclared.
6. **Per-agent scope applies.** `RunWorkflowTool::skill_allowlist` (`:215`) is the
   run-side twin of the read-side scoping in [`04`](04-per-agent-scoping.md); wire
   both from the same agent field so what an agent may read and what it may run
   cannot diverge.
7. **Per-lineage budgets**, not only process-global spawn and nesting caps.
8. **Scanning covers scripts.** [`05`](05-registry-trust-and-updates.md)'s scan is
   necessary and not sufficient for executable content; both ecosystems' worst
   incidents came from executable or install-time instructions
   ([`02`](02-industry-comparison.md)).

## The future execution path

Solid boxes exist today; dashed boxes are gaps. Numbers match "Requirements before
execution ships" above. None of this is wired, and nothing in this brief wires it.

```text
 an agent asks to run a skill  (not possible today)
                              │
                              ▼
 ┌──────────────────────────────────────────────────────────┐
 │ RunWorkflowTool (upstream, run_workflow.rs:215)          │
 │ skill_allowlist exists but is never passed               │  GAP 6
 └────────────────────────────┬─────────────────────────────┘  wire from the
                              │                                agent field (04)
                              ▼
 ┌ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─┐
 ┆ Config::load_or_init()  (run_machinery.rs:98, :210)      ┆  GAP 1
 ┆ reads a GLOBAL config, not the harness's                 ┆  injected config
 └ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─┬─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─┘
                              │
                              ▼
 ┌ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─┐
 ┆ metering: none on the run path                           ┆  GAP 2
 ┆ budget_usd_daily cannot bind a skill run                 ┆  usage sink
 └ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─┬─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─┘
                              │
                              ▼
 ┌ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─┐
 ┆ egress: allowed_domains = ["*"] when the list is         ┆  GAP 3
 ┆ empty  (run_machinery.rs:226-227)                        ┆  harness egress
 └ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─┬─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─┘  policy, deny
                              │
                              ▼
 ┌ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─┐
 ┆ upstream permission_level() == None  (:309-315)          ┆
 ┆ name "run_workflow" already parks (consequence.rs:447)   ┆  GAP 4 + 5
 ┆ run_skill would default to parks (consequence.rs:2534)   ┆  declare run_
 ┆ the spawned run's OWN steps: gate not shown              ┆  skill; gate
 └ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─┬─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─┘  inner steps
                              │
                              ▼
 ┌──────────────────────────────────────────────────────────┐
 │ ApprovalGate  (ports/approvals.rs:13)                    │
 └────────────────────────────┬─────────────────────────────┘
                              │
                              ▼
 ┌──────────────────────────────────────────────────────────┐
 │ spawned run                                              │
 │ GAP 7 per-lineage budgets · GAP 8 scan covers scripts    │
 └──────────────────────────────────────────────────────────┘

 ==============================================================================
 GATE: no skill tier model is designed before every gap above is closed
 ==============================================================================
```

- `RunWorkflowTool`: tool name `run_workflow.rs:41`, `skill_allowlist` `:215`,
  `permission_level()` `:309-315`.
- Config and egress: `Config::load_or_init()` `run_machinery.rs:98`, `:210`; the
  `["*"]` fallback `:226-227`.
- Policy: `run_workflow` is `Reach::Consequence` at `policy/consequence.rs:447`; an
  undeclared non-read tool defaults to the same at `:2534-2562`.
- Not wired: `harness/built_in/skills.rs:22-25`.

## Grant semantics to copy when it does ship

Claude's `allowed-tools` is the useful model (verified,
[`02`](02-industry-comparison.md)): a skill can **pre-approve** specific tools for
**one turn**; the grant **clears** afterwards; it does **not** restrict other tools;
and **deny and ask rules override it**. Copy the last two properties exactly. A
skill must never widen what the operator's grant chain allows, only smooth a call
that chain already permits.

Also weigh Gemini CLI's consent step (verified): before a skill's body is injected,
the user sees the skill name and the directory it gains access to. That fits
OpenCompany's human-in-the-loop posture for an execution-capable skill, and is out of
place for a read-only one.

## Explicit non-goals

- No `run_workflow` wiring, no new tool, no change to
  `dispatched_belt_excludes_every_deferred_family`.
- No skill tier model, per-skill risk labels or "safe/unsafe" flags.
- No use of upstream's `workflow_manage` write family from OpenCompany.

## Trigger to revisit

Reopen this file when either (a) OpenHuman lands an injection seam for config,
metering and egress, or (b) a concrete company need cannot be met by an orchestrator
running a workflow. At that point, design the gate against requirements 1–8 above,
not against MCP's tiers.
