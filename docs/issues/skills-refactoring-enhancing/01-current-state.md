# 01 — How skills work today

Read this to know what exists before changing it. All citations are under
`crates/opencompany-core/src/` unless prefixed `frontend/` or `vendor/`, and were
re-read against `upstream/main` `acaefbc63` on 2026-09-21.

## What a skill is

A skill is a `SKILL.md`: a `---`-fenced frontmatter block of `key: value` lines
followed by a Markdown body.

- Parsed shape: `SkillDoc { slug, name, description, category?, version?, body }`
  (`company/skill_file.rs:14`). `parse_skill_md` (`:40`) and `render_skill_md`
  (`:118`) round-trip it; the parser is hand-written, no YAML dependency.
- `version` is **descriptive only**: "nothing compares or orders it yet"
  (`skill_file.rs:25`). It rides inside a registry install's snapshot so a later
  "update available" affordance could diff it. That affordance does not exist.
- The body is preserved verbatim for OpenHuman's own parser.
- Frontmatter values are collapsed to one line (newlines become spaces) so a
  name or description cannot inject extra frontmatter keys or close the block
  (`server/ops/skills.rs:473-474`, pinned by
  `server/ops/skills_skill_md_frontmatter_resists_tests.rs`).

## Storage: two halves

| Half | Where | Written by |
| --- | --- | --- |
| Built-in content | disk: `companies/_globals/skills/*/SKILL.md` (embedded at build time, `[skills].always` in `globals.toml`) and `companies/<name>/skills/**` | the repo / bundle author; read-only at runtime |
| Operator deltas | `SkillStateStore` (`ports/skills_state.rs:47`), per company, keyed by slug | the write routes below |

`SkillState { slug, enabled, source, custom_doc? }` (`skills_state.rs:31`), with
`SkillSource::{Company, Registry, Custom}` (`:19`). A registry install stores the
**pinned snapshot** as `custom_doc`; a custom skill stores its authored document
there. A `Company`-source delta is only ever an enable/disable override.

A hosted tenant container has no repository checkout, so `skills_root()` is
`None` there and the shared registry is empty (`docs/spec/runtime/globals.md:26`).

## The effective set: one fold, three readers

`company::skill_effective::resolve` (`company/skill_effective.rs:132`) folds, bottom
to top: global baseline → company bundle → operator deltas, with
`[globals].disable = ["skill:…"]` entering as a synthesized disabling delta
(`globals_skill_disables`, `:92`) so there is no second opt-out mechanism. A
disable beats an enable. The harness, `GET …/skills` and the GraphQL
`Company.skills` resolver (`server/graphql/skills.rs`) all read this one
function, so the console cannot report a set the agents do not have.

`resolve` reports disabled entries too (the console needs the row to render the
switch); the harness filters to enabled ones.

## Lifecycle, route by route

Routes are registered by `router()` at `server/ops/skills.rs:101-108` under both
scope forms.

| Step | Route → function | Auth | Behaviour |
| --- | --- | --- | --- |
| List | `GET …/skills` → `list_skills` (`:265`) | `ScopedCompany` (any member) | the effective set, including disabled rows |
| Browse | `GET …/skills/registry` → `list_registry` (`:364`) | `ScopedCompany` | the shared, host-global library; metadata only, no body |
| Install | `POST …/skills/{slug}/install` → `install` (`:307`) | `AdminScopedCompany` | see below |
| Author | `POST …/skills` → `create_custom` (`:439`) | `AdminScopedCompany` | assembles a `SKILL.md`; capped at `MAX_SKILL_DOC_BYTES = 256 KiB` (`:55`) on the *assembled* document |
| Toggle | `PUT …/skills/{slug}` → `set_enabled` (`:404`) | `AdminScopedCompany` | read-modify-write under a per-company `write_lock` (`:87`) |
| Uninstall | `POST …/skills/{slug}/uninstall` → `uninstall` (`:377`) | `AdminScopedCompany` | only `Registry`/`Custom` skills; a `Company` bundle skill can only be disabled |

**Install is server-authoritative** (doc comment above `install`, `:286-306`):

1. slug in the registry → persist the library's document; the request body is
   ignored, and the snapshot is pinned (a later library edit does not rewrite an
   existing install);
2. slug absent from a **non-empty** registry → `404`;
3. **empty** registry → fall back to the client's metadata (hosted tenants, no
   `skills_root`), because refusing every install would break them;
4. a **configured** library that fails to load is a `500`, never case 3, so a
   broken library is not silently downgraded into client-authored content.

Every write is admin-only "since a skill's content becomes part of every agent's
effective prompt, company-wide" (`server/ops/skills.rs` module header). Reads are
open to any member.

## One install, end to end

One `POST …/skills/{slug}/install`, including the three ways the registry lookup
can go other than "found".

```text
 Browser          Router            Registry            Store          Pool
    │                │                  │                 │              │
    │ POST …/install │                  │                 │              │
    │───────────────►│                  │                 │              │
    │  AdminScopedCompany; slug must match [a-z0-9][a-z0-9-]*            │
    │                │                  │                 │              │
    │                │ ↻ take write_lock(company)         │              │
    │                │                  │                 │              │
    │                │ lookup slug      │                 │              │
    │                │─────────────────►│                 │              │
  alt ─ registry lookup outcome ──────────────────────────────────────────────
    │ [A] found: snapshot the LIBRARY document; request body IGNORED     │
    │                │                  │                 │              │
    │ [B] slug missing, registry NOT empty                │              │
    │ 404 not in registry               │                 │              │
    │◄───────────────│                  │                 │              │
    │                │                  │                 │              │
    │ [C] registry EMPTY (no skills_root): CLIENT metadata is used,      │
    │     persisted with source=Registry (see 05 §5.1)    │              │
    │                │                  │                 │              │
    │ [D] configured registry fails to load: hard 500, no downgrade      │
    │ 500            │                  │                 │              │
    │◄───────────────│                  │                 │              │
  end alt ────────────────────────────────────────────────────────────────────
    │                │                  │                 │              │
    │                │ check_skill_doc_size(≤256 KiB)     │              │
    │                │                  │                 │              │
    │                │ set(state{Registry,snapshot})      │              │
    │                │───────────────────────────────────►│              │
    │                │                  │                 │              │
    │ 200 InstalledSkill                │                 │              │
    │◄───────────────│                  │                 │              │
    │                │ (lock released)  │                 │              │
    │                │                  │                 │              │
  later ─ next agent cycle ───────────────────────────────────────────────────
    │                │                  │                 │              │
    │                │                  │                 │ list() deltas│
    │                │                  │                 │◄─────────────│
    │ ensure() (mod.rs:3151): deltas changed -> rebuild the roster       │
    │                │                  │                 │              │
    │ materialize() per agent (build.rs:1083); the agent then sees       │
    │ the skill in its catalogue and read tools           │              │
    │                │                  │                 │              │
```

- Handler `install` `server/ops/skills.rs:307`; slug check `:313`; lock `:319`;
  registry load `:321` (a load error is case [D], the `500`); the not-found `404`
  `:324-326`; the empty-registry fallback `:329-341`; size check `:343`; store
  write `:350`.
- The fallback persists `source: SkillSource::Registry` (`:347`), the same value a
  real library install gets — see [`05`](05-registry-trust-and-updates.md) §5.1.
- Refresh: `HarnessPool::ensure` `harness/built_in/mod.rs:3151`; `materialize`
  `harness/built_in/skills.rs:70`, called from `build.rs:1083`.

## Materialization: what an agent actually gets

`EffectiveSkills::materialize` (`harness/built_in/skills.rs:70`) is called once
per agent from `harness/built_in/build.rs:1083`, into
`<workspace_root>/<company>/<agent>/skill-catalog/` (`build.rs:1076`,
`docs/spec/runtime/workspace-layout.md:135`). It writes each enabled skill's
`SKILL.md` and bundled resources to `skill-catalog/skills/<slug>/`
(`skills.rs:78`) and **rebuilds the tree from scratch every call**, so a removed
skill disappears. `HarnessPool::ensure` re-drives it whenever the deltas change,
so a console change reaches every agent on the next cycle, no restart
(`skills.rs:10-20`).

The agent receives:

- three **read** tools wrapping OpenHuman's `WorkflowListTool`,
  `WorkflowDescribeTool`, `WorkflowReadResourceTool` (`skills.rs:35`, `:148-161`),
  named `list_skills`, `describe_skill`, `read_skill_resource`;
- a plain-text catalogue appended to its persona prompt, headed "Skills available
  to you (read-only)" (`skills.rs:169-193`).

All three tools are `EffectGroup::Other, Reach::Nothing`
(`policy/consequence.rs:667-669`) — the same bucket as any local metadata read.

The build step is best-effort: a failure logs
`skill catalogue unavailable for this agent` and the agent still answers
(`build.rs:1083-1100`).

## Read-only by construction

`skills.rs:22-25`: skill *execution* (`run_workflow`) "is not wired here.
`RunWorkflowTool` reaches for the global `Config::load_or_init()` and bypasses
the harness's metering, so it needs an upstream injection seam that does not
exist yet." The console's read-only banner and "Agents can read this" label
(`frontend/src/lib/skills.ts:45`, `:56`) are a copy-layer fix for the same fact
(issue #569). See [`07-execution-deferred.md`](07-execution-deferred.md).

## The seven gaps

1. **No per-agent or per-desk scope.** `skills.rs:18` states a skill "surfaces to
   every agent on the next cycle". `materialize` takes no agent id
   (`skills.rs:70`), although its one caller, `build.rs:1083`, has
   `manifest_agent` in scope. Tools get a three-level grant
   (`docs/spec/runtime/tools.md`); skills get nothing analogous. This is **not** a
   documented deferral, unlike MCP's #1270. Upstream OpenHuman already ships the
   mechanism: `RunWorkflowTool::skill_allowlist` and `with_skill_allowlist`
   (`vendor/openhuman/crates/openhuman-core/src/agent/tools/run_workflow.rs:215`,
   `:246`); `grep -rn skill_allowlist crates/opencompany-core/src` returns nothing.
   See [`04-per-agent-scoping.md`](04-per-agent-scoping.md).
2. **No install-time scanning.** `install` snapshots the library document and
   `create_custom` assembles one; neither inspects content. Description and
   catalogue text reach the prompt verbatim. See
   [`05-registry-trust-and-updates.md`](05-registry-trust-and-updates.md).
3. **Absent from the threat model.** `grep -ci skill` returns `0` for
   `docs/spec/security/agent-isolation.md`, `docs/spec/company-brain/grants.md`,
   `docs/spec/company-brain/approvals.md` and `docs/spec/runtime/tools.md`.
   Registry-authored `SKILL.md` content read via `describe_skill` /
   `read_skill_resource` is untrusted text by the repo's own standard for a fetched
   page or an MCP tool description. See [`03-prerequisites.md`](03-prerequisites.md).
4. **No drift signal.** `version` is stored, never compared (above).
5. **Naming collision.** `[place].skills = [{ id, price_usd, description }]`
   (`docs/spec/runtime/manifest.md:172`) prices tiny.place A2A capabilities and is
   served at `GET /a2a/{handle}/skill.md` (`docs/spec/runtime/api.md:388`). It shares
   the word, and nearly the filename, with the `SKILL.md` bundle and nothing
   signposts the difference.
6. **No owning doc.** There is no `docs/spec/runtime/skills.md` and no
   `docs/modules/skills.md`, although `docs/modules/mcp.md` exists. Skills appear
   in passing across `globals.md`, `ports-console.md` (`:227-241`),
   `workspace-layout.md`, `harnesses.md`, `api*.md`, `desktop.md` and others. The
   fragments are consistent with each other; the gap is coverage, not accuracy.
7. **Authoring is thin.** The Add dialog (`frontend/src/views/SkillsView.tsx:503-621`)
   takes name, category, one-line description and a Markdown "Playbook"; no
   upload, no bundled files, no AI-assisted draft, no Filter/Sort. See
   [`06-authoring-ux.md`](06-authoring-ux.md).

## The console today

`frontend/src/views/SkillsView.tsx` (621 lines), `frontend/src/api/skills.ts`
(119), `frontend/src/lib/skills.ts` (76). Two tabs, "Installed" and "Registry"
(`SkillsView.tsx:86-87`); registry and installed reads fail independently
(`Promise.allSettled`, `:160`); writes need a resolved `canManage`. A skill's switch
is company-wide on/off — there is no per-agent surface. Skills sits under
Connections because "installing a skill is the same act as connecting an app"
(`frontend/src/views/settings-pages.ts:89`).

## Not a gap

Install and uninstall are not `ApprovalGate`-gated. That is consistent: they are
admin configuration actions, not agent effects crossing the trust boundary.
