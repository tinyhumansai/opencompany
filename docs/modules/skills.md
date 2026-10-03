# Skills (packaged procedures an agent reads)

A skill is a `SKILL.md` document — frontmatter plus a Markdown body — that a
company installs and every one of its agents can then **read**. It is not a
capability. Installing one adds text to an agent's reach; it adds no tool, no
credential and no grant.

This is the owning page for the subsystem. Skills were described in passing
across [`globals.md`](../spec/runtime/globals.md),
[`ports-console.md`](../spec/runtime/ports-console.md),
[`workspace-layout.md`](../spec/runtime/workspace-layout.md),
[`harnesses.md`](../spec/runtime/harnesses.md) and the API pages; those
fragments agree with each other, so this page consolidates rather than corrects.

## The document

```rust
pub struct SkillDoc {
    pub slug: String,
    pub name: String,
    pub description: String,
    pub category: Option<String>,
    pub version: Option<String>,
    pub body: String,
}
```

`parse_skill_md` and `render_skill_md`
([`company/skill_file.rs`](../../crates/opencompany-core/src/company/skill_file.rs))
round-trip it. The parser is hand-written — a `---`-fenced block of
`key: value` lines — with no YAML dependency, and it preserves the body verbatim
so OpenHuman's own parser sees exactly what was authored.

`version` is **descriptive only**. Nothing compares or orders it. It rides
inside a registry install's snapshot so that a future "update available"
affordance could diff it; that affordance does not exist today.

Frontmatter values are collapsed to a single line — newlines become spaces —
when a document is assembled, so a `name` or `description` cannot inject an
extra frontmatter key or close the block. That behaviour is pinned by
`server/ops/skills_skill_md_frontmatter_resists_tests.rs` and must survive any
later validation work.

### Spec deltas worth knowing

The published Agent Skills format requires `name` ≤ 64 characters, lowercase
letters, digits and hyphens, matching the directory, no leading or trailing
hyphen and no consecutive hyphens, and `description` ≤ 1024 characters. What
this repo enforces today is narrower:

- `skill_effective::valid_slug` requires a leading lowercase letter or digit
  followed by lowercase letters, digits or hyphens, and exists to stop path
  traversal (a slug becomes a directory name). It sets **no length cap**, and
  it constrains only the first character and the alphabet — so `deploy-` and
  `deploy--helper` both pass here and neither is a legal published name.
- `parse_skill_md` treats `name` as a display string that only has to be
  non-empty, ignores unknown keys, and does **not** check that `name` matches
  the slug.
- `MAX_SKILL_DOC_BYTES` (256 KiB) bounds only the *assembled* document.

So the 64-character name limit, the name-equals-directory rule, the hyphen
placement rules and the 1024-character description limit are recorded here as
known gaps, not as enforced rules.

## The four sources

1. **Global baseline** — `[skills].always` in
   `companies/_globals/globals.toml`, whose `SKILL.md` bundles live at
   `companies/_globals/skills/<slug>/` and are embedded at build time. Installed
   in every company, including a hosted tenant with no repository checkout.
   Ships `web-research`, `weekly-report` and `meeting-brief`.
2. **Company bundle** — `companies/<name>/skills/**` in the company's own
   directory. Read-only at runtime; the operator can disable one but never
   uninstall it.
3. **Shared registry** — the host-global library an operator browses and
   installs by slug. A hosted tenant container carries no repository checkout,
   so `AppConfig::skills_root()` is `None` there and this library is **empty**.
4. **Console-authored custom** — a skill the operator writes in
   `Settings → Skills`, stored as a whole `SKILL.md` in the delta.

Sources 1 and 2 are content on disk. Sources 3 and 4 arrive as operator deltas.

## Storage: two halves

| Half | Where | Written by |
| --- | --- | --- |
| Content | disk — the embedded baseline and `companies/<name>/skills/**` | the repo or the bundle author; read-only at runtime |
| Operator deltas | `SkillStateStore` ([`ports/skills_state.rs`](../../crates/opencompany-core/src/ports/skills_state.rs)), per company, keyed by slug | the write routes below |

```rust
pub struct SkillState {
    pub slug: String,
    pub enabled: bool,
    pub source: SkillSource,     // Company | Registry | Custom
    pub custom_doc: Option<String>,
}
```

A registry install stores the **pinned snapshot** as `custom_doc`. A custom
skill stores its authored document there. A `Company`-source delta is only ever
an enable/disable override — it carries no document, because the document is on
disk and the operator does not own it.

Company A's deltas MUST be invisible to company B; that is stated on the trait,
and the storage conformance suite holds every backend to it.

## The effective set: one fold, three readers

`company::skill_effective::resolve`
([`company/skill_effective.rs`](../../crates/opencompany-core/src/company/skill_effective.rs))
folds bottom to top:

```text
global baseline  →  company bundle  →  operator deltas
```

with these rules:

- a company bundle is enabled unless a delta disables it;
- a delta sets the slug's `enabled` flag and its provenance, and a delta
  carrying a `custom_doc` supersedes the document beneath it;
- a delta with no `custom_doc` contributes no document of its own;
- **a disable anywhere wins over an enable**;
- a malformed `custom_doc`, or a delta whose slug is not a safe directory name,
  contributes nothing rather than failing the whole resolution;
- a malformed *company bundle* **does** fail it — `load_dir_skills` refuses the
  whole directory, because a reader answering with the surviving subset would
  describe a set no agent actually has.

### `[globals].disable` is not a second mechanism

A manifest's `[globals].disable = ["skill:meeting-brief"]` does not get its own
opt-out path. `globals_skill_disables` turns each `skill:` entry into a
**synthesized disabling delta**, which then enters the same fold as any operator
delta and loses to nothing. One fold, one precedence rule, one place to reason
about. See [`globals.md`](../spec/runtime/globals.md) for the rest of the
`disable` vocabulary.

### Three readers, one derivation

The harness, `GET …/skills` and the GraphQL `Company.skills` resolver
([`server/graphql/skills.rs`](../../crates/opencompany-core/src/server/graphql/skills.rs))
all call `resolve`. The console therefore cannot report a set the agents do not
have.

`resolve` reports **disabled** entries too — the console needs the row to render
its switch. The harness filters to the enabled ones.

## Per-agent scope: the second operand

The company's enabled set says what exists. A teammate's `skills` list says
which of it that teammate reads. The effective answer is the intersection, and
it is **narrow-only by construction**: a scope filters the enabled set, so it
can never confer a skill the company has not enabled.

The field carries three states, the same shape a teammate's tools use:

| Stored | Means |
| --- | --- |
| absent (`None`) | inherits — every skill the company enables, including ones enabled later |
| a list | exactly those slugs, intersected with the enabled set |
| `[]` | deliberately none |

`inherits` and `[]` both read nothing while a skill is disabled, and only the
first takes it back when the switch returns — so the two are never collapsed.

`agent_effective_skills`
([`runtime/builder.rs`](../../crates/opencompany-core/src/runtime/builder.rs))
is the one derivation; `resolve_for_agent` and the team route's `agent_skills`
both call it, so the console cannot report a scope the harness does not apply.

Entries match **exactly**. A trailing-`*` prefix was considered and refused: a
prefix silently admits a skill installed after the scope was written, which is
the one direction a narrowing mechanism must not move on its own. The route
rejects a `*` rather than storing it.

A stored slug the company does not have enabled is kept rather than refused, and
reported as dropped (`unmet_scope_slugs`), so a scope that confers nothing says
so instead of looking empty.

### Enforced where the files are written

Scope is applied at **materialization**, not by hiding rows from a catalogue.
`build.rs` passes the teammate's list into `EffectiveSkills::materialize`, which
filters before the write loop — so a skill outside an agent's scope is never
written into that agent's `skill-catalog/` at all, and `read_skill_resource`
cannot reach it however the agent asks. `skills_scope_tests.rs` asserts that
directly, from the other agent's side.

### Who reaches a skill

`agents_for_skill`
([`company/skill_scope.rs`](../../crates/opencompany-core/src/company/skill_scope.rs))
inverts the same arithmetic into a per-skill answer, so `GET …/skills` can say
who each row reaches. It is a read-side projection only — the write is always to
the teammate's own field.

## What a document must pass before it is stored

Three gates, shared by every write path and living together in
[`server/ops/skills/vet.rs`](../../crates/opencompany-core/src/server/ops/skills/vet.rs):
the size cap, the scanner's verdict, and the per-company write lock.

`scan_skill`
([`company/skill_scan.rs`](../../crates/opencompany-core/src/company/skill_scan.rs))
runs on install, authoring, upload, draft and update — every path that can put
text in front of an agent. It **warns** by default and **blocks** on three
classes: invisible or bidirectional code points, hard-coded credentials, and a
resource path that escapes the skill directory. A block writes nothing.

The override is a per-request `force` on that one write, never a host-wide
setting. A switch that silences a class of finding for a whole host is an alarm
turned off; a `force` is a decision an operator makes about one document, and it
is recorded with it.

Whatever survives, the prompt catalogue still renders name and description as
**quoted data** with invisible code points stripped and lengths capped, so a
warn-level document cannot restructure the prompt around it.

## Provenance: what an install pinned

An install snapshots the library's document and records what it pinned —
digest, version, who installed it, when
([`ports/skills_state.rs`](../../crates/opencompany-core/src/ports/skills_state.rs)).
The company reads its own copy from then on, so the library can move without
changing what agents read.

Two comparisons fall out of that pin, and they are separate answers:

| Field | True when | Meaning |
| --- | --- | --- |
| `updateAvailable` | the library's current digest ≠ the pinned digest | somebody republished the skill upstream |
| `modified` | the stored copy's digest ≠ the pinned digest | somebody edited this company's copy |

Both can hold at once. `POST …/skills/{slug}/update` re-pins onto the library's
current document and re-runs the scan — and **refuses while `modified`**, with a
`409`, because applying it would discard an operator's edit with nothing to
recover it from. Nothing ever updates on a read: a pinned install stays pinned
until an admin acts.

An empty registry means the host serves no shared library at all, so that
install is recorded as `Custom` rather than `Registry` — the client's own
metadata is not a library snapshot, and labelling it one would make an
uncheckable row indistinguishable from a checkable one.

Every write that changes a skill's document appends a `SkillChanged` row —
slug, change, tier, digest, actor, and never the body. A failed append fails the
write, so the store and the journal cannot disagree.

## Lifecycle, route by route

Routes are registered by `router()` in
[`server/ops/skills.rs`](../../crates/opencompany-core/src/server/ops/skills.rs)
under both scope forms (`…/companies/{id}/…` and the `…/company/…` prosumer
alias).

| Step | Route → function | Auth | Behaviour |
| --- | --- | --- | --- |
| List | `GET …/skills` → `list_skills` | `ScopedCompany` (any member) | the effective set, including disabled rows |
| Browse | `GET …/skills/registry` → `list_registry` | `ScopedCompany` | the shared library; metadata only, no body |
| Install | `POST …/skills/{slug}/install` → `install` | `AdminScopedCompany` | server-authoritative, below |
| Author | `POST …/skills` → `create_custom` | `AdminScopedCompany` | assembles a `SKILL.md`, capped at `MAX_SKILL_DOC_BYTES` = 256 KiB on the assembled document |
| Toggle | `PUT …/skills/{slug}` → `set_enabled` | `AdminScopedCompany` | read-modify-write under the per-company write lock |
| Uninstall | `POST …/skills/{slug}/uninstall` → `uninstall` | `AdminScopedCompany` | `Registry` and `Custom` only; a `Company` bundle skill can only be disabled |
| Upload | `POST …/skills/upload` → `upload` | `AdminScopedCompany` | multipart `.md` / `.zip` / `.skill`, one result row per file; archive hardening in [`company/skill_upload.rs`](../../crates/opencompany-core/src/company/skill_upload.rs) |
| Draft | `POST …/skills/draft` → `draft` | `AdminScopedCompany` | writes nothing; returns a drafted document, scanned before it is shown |
| Update | `POST …/skills/{slug}/update` → `update` | `AdminScopedCompany` | re-pins an install onto the library's current document, re-running the scan; refuses a locally edited copy |
| Read a document | `GET …/skills/{slug}/doc` → `read_doc` | `ScopedCompany` (any member) | the whole `SKILL.md` an agent reads, resolved through the same effective set `GET …/skills` reports; a bundle entry off disk |
| Rewrite a document | `PUT …/skills/{slug}/doc` → `write_doc` | `AdminScopedCompany` | `Registry` and `Custom` only, the same arms uninstall accepts; carries the install pin through, so the edit reports as `modified` |

`/skills/registry` is a static segment, so it wins over the `{slug}` pattern
regardless of registration order — and the methods differ anyway.

### Every write is admin-only

A skill's content becomes part of every agent's effective prompt, company-wide,
so installing, uninstalling, toggling or authoring one decides something for the
company rather than for the caller alone. The two reads stay open to any member;
only the writes decide anything.

### The per-company write lock

`set_enabled` and its siblings are read-modify-write over the delta list, so the
handlers serialize per company on a `write_lock(company)` mutex. Two admins
toggling two different skills at the same moment would otherwise each write back
a list that does not contain the other's change. The lock is in-process, which
covers the deployed topology: a tenant is a single container.

### Install is server-authoritative

The request body is ignored whenever the library can serve the slug — a client
cannot dictate what a registry skill contains. Resolution, in order:

1. **Slug in the registry** → persist that document. The snapshot is **pinned**:
   a later library edit does not rewrite an existing install.
2. **Slug absent from a non-empty registry** → `404`. That is a typo or a stale
   client, and silently persisting a stub is what produced content-less installs
   before.
3. **Empty registry** → fall back to the client's metadata. An empty registry
   means this host serves no shared library at all (platform-provisioned, no
   `skills_root`), so there is nothing to resolve against and refusing every
   install would break hosted tenants outright.
4. **A configured library that fails to load** → `500`, never case 3. Silently
   degrading a broken library to "no library" would hand the client authorship
   of a registry skill's contents on exactly the hosts that meant to be
   server-authoritative.

Case 3 persists `source: SkillSource::Registry`, the same value a real library
install gets — a known provenance ambiguity, since the two are then
indistinguishable on read.

## Materialization: what an agent actually gets

`EffectiveSkills::materialize`
([`harness/built_in/skills.rs`](../../crates/opencompany-core/src/harness/built_in/skills.rs))
is called once per agent from `harness/built_in/build.rs`, into

```text
<workspace_root>/<company>/<agent>/skill-catalog/skills/<slug>/
```

It writes each **enabled** skill's `SKILL.md` and bundled resources there and
**rebuilds the tree from scratch on every call**, so a removed skill disappears
rather than lingering. `HarnessPool::ensure` fetches the deltas at the top of
each cycle and rebuilds the roster when they differ, so a skill authored,
enabled or disabled in the console reaches every agent on the next cycle with no
process restart. An unchanged delta set is a no-op: the cached roster, and each
agent's conversation state, is left in place.

The build step is **best-effort**. A failure logs `skill catalogue unavailable
for this agent` and the agent still answers — the branch also runs for a company
whose workspace root is unusable, where failing here would turn "this agent has
no skill catalogue" into "this company cannot build an agent at all".

An agent gets a skill catalogue when the harness is wired to a skills source, or
the company has operator deltas, or the global baseline is non-empty. That last
clause is what reaches a platform-provisioned tenant, which has neither of the
first two.

### Three read tools

`read_tools()` wraps OpenHuman's `WorkflowListTool`, `WorkflowDescribeTool` and
`WorkflowReadResourceTool` in a `Config` pointed at the materialized tree, and
renames them through `naming::skill_read_tools` to `list_skills`,
`describe_skill` and `read_skill_resource`. They are **read** tools and nothing
else: list the catalogue, inspect one entry, read a bundled file.

All three are classified `EffectGroup::Other, Reach::Nothing` in
[`policy/consequence.rs`](../../crates/opencompany-core/src/policy/consequence.rs)
— the same bucket as any other local metadata read.

### The prompt catalogue

`catalogue()` appends a plain-text list to the persona body, headed "Skills
available to you (read-only). Each is a packaged, reusable procedure:", one line
per skill (`- {name} (\`{slug}\`): {description}`), then the sentence that hands
the agent the three tool names. The catalogue is folded into the persona body
because `SystemPromptBuilder::for_subagent`'s `omit_skills_catalog` flag is
inert upstream and cannot be relied on. An empty set produces an empty string,
so an agent with no skills gets no catalogue at all.

That sentence also says a skill is **not** one of the company's saved
workflows — those are stored graphs, listed on the Workflows page, and none of
the three tools can see them. The tools were renamed for exactly that confusion.

## Execution is deliberately not wired

An agent can read a skill. It cannot run one. `run_workflow` is not wired into
the harness, and this is a stated deferral rather than an oversight.

The reason is a missing seam, not a missing feature flag. OpenHuman's
`RunWorkflowTool` reaches for the global `Config::load_or_init()` and bypasses
the harness's metering, so wiring it as-is would give a tenant agent a code path
that reads configuration the host did not hand it and spends money the host does
not count.

**The gate that has to be met first** is an upstream injection path that carries
three things into a skill run:

- **configuration** — the run must take the host's `Config`, not load its own;
- **metering** — a skill run's spend must pass through the same accounting as
  every other turn, or a company's budget stops being a ceiling;
- **egress** — a skill that runs is a skill that can reach the network, so it
  must land inside whatever egress policy the tenant has, and today that is
  [none](../spec/security/agent-isolation.md#4-there-is-no-egress-policy).

Until all three exist upstream, the answer is a read-only catalogue. The
console's read-only banner and its "Agents can read this" row label say the same
thing to the operator.

The security posture in
[`agent-isolation.md`](../spec/security/agent-isolation.md#skills-are-text-an-agent-reads)
is written against this fact. It changes the day execution ships.

## Known gaps

| Gap | Effect today |
| --- | --- |
| No per-desk scope | A scope is the company's enabled set intersected with one teammate's list. There is no desk level between them |
| Trust tier is computed but not published | `trust_tier` reaches the journal row; both read APIs expose `source`, so a baseline skill and a company-bundle one carry the same console label. Installer and install time are stored and never returned |
| No diff before an update | No route serves a skill's body, so the review names the two revisions rather than showing what changes |
| No dry run | A verdict is only ever produced by a write attempt: a block returns the report, writes nothing, and offers a per-request `force` |
| Scope changes and scan verdicts are not journalled | `SkillChange` covers `Installed` / `Updated` / `Removed`; a scope edit goes through the team route, which appends no company event |
| Validation narrower than the spec | See [Spec deltas](#spec-deltas-worth-knowing) |

## Where this lives

| Concern | File |
| --- | --- |
| Document shape, parse and render | `crates/opencompany-core/src/company/skill_file.rs` |
| The effective-set fold, slug validation, `[globals].disable` synthesis | `crates/opencompany-core/src/company/skill_effective.rs` |
| Operator deltas (port + conformance) | `crates/opencompany-core/src/ports/skills_state.rs` |
| Write routes | `crates/opencompany-core/src/server/ops/skills.rs`, and `skills/` beside it for registry, upload, draft and update |
| Size cap, scan verdict, per-company lock | `crates/opencompany-core/src/server/ops/skills/vet.rs` |
| Content scanning | `crates/opencompany-core/src/company/skill_scan.rs` |
| Shared validation | `crates/opencompany-core/src/company/skill_validate.rs` |
| Archive hardening | `crates/opencompany-core/src/company/skill_upload.rs` |
| Install pin, drift, trust tier | `crates/opencompany-core/src/company/skill_provenance.rs` |
| Per-agent scope: the derivation | `crates/opencompany-core/src/runtime/builder.rs` |
| Per-agent scope: the per-skill inversion | `crates/opencompany-core/src/company/skill_scope.rs` |
| `Company.skills` and `skillRegistry` reads | `crates/opencompany-core/src/server/graphql/skills.rs` |
| Materialization, read tools, prompt catalogue | `crates/opencompany-core/src/harness/built_in/skills.rs` |
| Tool consequence classification | `crates/opencompany-core/src/policy/consequence.rs` |
| Console surface | `frontend/src/views/SkillsView.tsx`, `frontend/src/views/skills/`, `frontend/src/api/skills.ts`, `frontend/src/lib/skills.ts` |

The harness half compiles only under `--features openhuman`; the document,
fold and port halves are always compiled.

## Related

- [`../spec/runtime/globals.md`](../spec/runtime/globals.md) — the global
  baseline and `[globals].disable`
- [`../spec/runtime/tools.md`](../spec/runtime/tools.md) — the three-level tool
  grant a skill's text must still pass through
- [`../spec/security/agent-isolation.md`](../spec/security/agent-isolation.md)
  — the threat model, including the skills section
- [`mcp.md`](mcp.md) — the neighbouring subsystem that *does* confer capability,
  and is scoped and gated accordingly
