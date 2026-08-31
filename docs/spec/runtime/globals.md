# The global baseline

A bundle under `companies/<name>/` describes one vertical — a law firm, a game
studio, a recruiting company. The **global baseline** is the part that is the
same in all of them: the teammates, workflow graphs, and skills every company
has whichever vertical it was started from, plus the tool belt it starts with.

The authored source is `globals/`, beside `companies/`:

```text
globals/
  globals.toml      # [tools].default_allow, [skills].always
  agents/*.toml     # one file per global teammate
  workflows/*.toml  # one file per global graph
  ledgers/*.toml    # one file per baseline ledger
```

The code is [`crate::globals`](../../../src/globals/mod.rs), and the shipped
contents are described in [`globals/README.md`](../../../globals/README.md).

## Embedded, not read from disk

Everything in `globals/` is embedded into the binary by `build.rs`, exactly like
the built-in ledgers in `src/ledger/registry.rs`. A platform-provisioned tenant
container carries no repository checkout — it is why `skills_root()` is `None`
there and the shared skill registry is empty — so a baseline resolved from the
filesystem would be a baseline every hosted company silently lacked.

Malformed input is a **fault**, never a panic: `globals::faults()` reports it and
the rest of the baseline still loads, so one broken global never costs a company
the other three.

## What each surface does

| Surface | Where it merges | Rule |
| --- | --- | --- |
| Agents | `CompanyManifest::apply_globals` | Appended after the company's own roster; an id the company declares is skipped. |
| Workflows | `list_workflows_with_globals` / `load_workflow_with_globals` | Seed file, then saved overlay, then the global graph. |
| Skills | `EffectiveSkills::materialize` | Installed as the bottom layer; a company bundle or `custom_doc` delta of the same slug supersedes it. |
| Ledgers | `runtime::builder::seed_ledgers` | **Seeded once** into the company's own store at first boot, then owned by the company. A bundle declaration of the same slug replaces the global before either is stored. |
| Setup cards | `runtime::builder::seed_tasks` | **Seeded once** onto the board at first boot, in To-do. A bundle card of the same id replaces the global one. Opt-in per caller — see below. |
| Tools | `Tools::default` | `[tools].default_allow` is the belt a company with no `[tools]` section gets. |

### A company always wins

Nothing merges field by field. On an id collision the company's own definition
replaces the global one outright, because half a teammate and half a graph are
nobody's design. To drop a global rather than replace it, the manifest says so:

```toml
[globals]
disable = ["agent:researcher", "workflow:weekly_review", "skill:meeting-brief", "ledger:risks", "task:name-the-top-risks"]
```

Every entry is `<kind>:<id>` with a kind from `globals::DISABLE_KINDS`, and must
name a global that exists — a typo is a validation error, not a line that
silently does nothing. `skill:` entries reach the effective skill set as
synthesized disabling deltas (`harness::globals_skill_disables`) so the manifest
and the console's own toggle speak the same vocabulary; a disable beats an
enable, so the manifest wins over a console re-enable.

### Ledgers and setup cards are seeded rather than resolved

Every other surface above is re-resolved on each read, so editing `globals/`
changes what an existing company gets on its next load. Ledgers cannot work that
way, because a company **owns its record**: `docs/spec/runtime/ledgers.md` makes
retiring a ledger a person's call, and a baseline re-applied on every boot would
undo that call on the next restart. A `ledger:` disable entry therefore governs
what a *new* company is seeded with, not what an existing one keeps.

### Setup cards, and why they are opt-in

`globals/tasks.toml` is the setup work every company starts with on its board —
write the brief, set the first goals, record the standing decisions, name the
top risks, wire the connections — on top of which each bundle's own
`tasks.toml` adds the setup its vertical is defined by. Seeded once, at first
boot, into To-do.

Two properties are load-bearing:

- **A seeded card can never dispatch.** A seed file has no `column` key at all,
  and the seeder writes through the plain `TaskStore`, not
  `CompanyRuntime::upsert_task` — the single site that edge-fires a dispatch
  (`in_progress`) or a billed planning pass (`planning`). Either alone would be
  enough; both are there because a freshly provisioned company spending
  inference on work nobody asked for is the failure worth paying twice to avoid.
- **Seeding is opt-in**, unlike ledger seeding, which is unconditional. Cards
  are visible state that tests count: `tests/one_card_per_message.rs` asserts
  exact board sizes against a company built straight from `RuntimeBuilder`, and
  a baseline arriving unasked would quietly turn those assertions into
  statements about the baseline. `RuntimeBuilder::with_task_seeding(true)` is
  set by the product entry points and nothing else.

The first-boot gate is `store.load(&id)` returning `None` — the last moment a
first boot is distinguishable, since the `store.save` at the end of the build
makes every later boot a returning one. That is stricter than the ledger
seeder's "nothing declared yet" gate, and deliberately: clearing the board is
routine, and a card an operator deleted reappearing on the next restart is the
runtime arguing with them.

Seeding runs only when the company has no declaration at all, and never on a
rebuild. The honest limit that follows: a person who retires *every* declared
ledger and then restarts is seeded again. That is visible and reversible, and it
is a better failure than a baseline that silently re-asserts one ledger somebody
deliberately dropped.

Each declaration is admitted through `ledger::Registry` before it is stored, so
the cap and the collision rules apply to the baseline exactly as they do to a
`define_ledger` call. A refused declaration is a warning in the log, not a boot
failure — a company must reach the rest of itself when one axis is bad.

### Why globals load last, and never orchestrate

`orchestrator_id` picks the first agent tagged `tier = "orchestrator"`, and
falls back to the first agent declared. So the baseline is appended **after** the
company's roster, and a global tagged `orchestrator` is dropped with a fault. Two
rules, one guarantee: which teammate runs the company is decided by the company.

For the same reason, when a role label in a drafted workflow matches both a
company teammate and a global one (`writer` is both a baseline id and a common
company role), the company's teammate wins rather than the drafter reporting an
ambiguity — see `harness::workflow_build::resolve_agent_ids`.

### Global teammates ask for their tools

Each `globals/agents/*.toml` declares an explicit `tools` list. An agent that
requests nothing inherits the company-wide belt whole, which for a global
teammate would mean every vertical's MCP servers, Composio account, and media
budget — granted to a teammate that company never wrote. A request is intersected
with `[tools].allow`, so a global's belt can only ever be narrower than what the
company already permits.

### Tools are a default, not a floor

`[tools].default_allow` is what a company gets when its manifest declares no
`[tools]` section. It is deliberately not a floor granted on top of whatever a
company allows: `workspace` confers workspace *writes* that not even a `*` grant
does, and an agent with no `files` grant is meant to be offered no file tools at
all, so a floor would silently re-grant authority a company withheld on purpose.
What is global here is where the starting belt is authored.

The belt is **wide**: `["*", "workspace.*", "workspace.write", "media",
"composio", "search", "mcp:*"]` — every namespace a company can hold except the
credential-gated `chargebee`, `hosting` and `paypal` integrations, and `repo`,
which an fs-storage host refuses to boot with. `workspace.write` is
listed explicitly because `workspace.*` is a *read* grant and a bare `*` does
not confer writes; an operator narrowing this list must copy it verbatim or the
company's teammates silently lose the write tier. Turning one off is a company-level edit
to `[tools].allow` (which replaces this list rather than extending it), and
that is the only off switch there is. It used to be narrow, and what that
bought was first-run companies whose teammates reported their own tools as "not
enabled" for capabilities the operator had already installed — see
`docs/spec/runtime/tools.md`.

## Provenance, and why it is persisted

`Agent::global` and `WorkflowFile::global` mark what came from the baseline.

A manifest is serialized back into the store with the merged roster in it, so
without a marker a global teammate would be indistinguishable from one the
company wrote — and the baseline could never be updated, retired, or opted out of
for that company again. With it, `apply_globals` is idempotent: it drops every
previously-merged global and re-appends the current baseline. That is what makes
a baseline change, or a `[globals].disable` written later, take effect on the
next read rather than at the next reprovision.

Every store backend therefore reads through `CompanyManifest::from_stored_toml`
rather than a bare `toml::from_str`: a company is provisioned once and read
thereafter, so a baseline applied only where bundles are parsed would reach new
companies and no existing one.

`Agent::global` also travels to the console, on every `GET …/team` row. That is
not decoration: because the baseline is merged into every company, "is this
roster empty?" is false everywhere, and the console's first-run gate asked
exactly that — so [company setup](company-setup/overview.md) could not open on any
company, including the fixture built to reach it (issue #1404). The gate now
asks whether any teammate is *not* from the baseline, which is a question only
the provenance marker can answer. A console re-deriving it from a copied list of
global ids would break on the next global added, silently.

## Adding to the baseline

1. Add the file under `globals/agents/` or `globals/workflows/`, or the slug to
   `[skills].always` (its `SKILL.md` must exist in `skills/`).
2. An agent node in a global workflow may only name a **global** agent — the
   graph runs in companies whose rosters it has never seen. A test enforces it.
3. Run `cargo test --features openhuman globals::` — the baseline's own tests
   check it parses, carries no orchestrator, and references nothing missing.
4. Expect roster, workflow-listing, and skill-set counts in other tests to move:
   every company gains what you added.
