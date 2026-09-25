# Agent definitions

*How a teammate is declared, and what reaches its system prompt.*

Terms: [glossary](../glossary.md). Tool scoping is
[tools.md](tools.md); which workspace documents a role is routed is
[orchestration/context-routing.md](orchestration/context-routing.md).

---

## Two authoring forms

A company's roster may be written either way:

**Inline** — `[[agent]]` blocks in `company.toml`. Unchanged, still valid, still
the smallest thing that works.

**Per file** — one `agents/<id>.toml` per teammate under the company bundle.

```
companies/acme/
├── company.toml          # everything except the roster
├── agents/
│   ├── copywriter.toml   # the id is the filename
│   ├── seo_specialist.toml
│   └── prompts/
│       └── house-style.md
└── workspace/
```

The per-file form exists because a teammate is more than four fields once it
carries a custom prompt and its own briefing documents. A multi-line TOML string
inside an array-of-tables is unreadable at roster length, and prose belongs
beside the agent it configures.

### The two forms are exclusive

A bundle with both an `agents/` directory and `[[agent]]` entries is a
**validation error**, not a precedence rule. Either precedence rule silently
discards teammates an operator wrote down, and the roster is the one part of a
manifest where a silent omission stays invisible until the missing teammate
fails to answer.

### The filename is the id

`agents/copywriter.toml` declares the agent `copywriter`. An `id` key inside the
file is accepted only when it agrees with the stem; a mismatch is an error
naming both, because silently preferring one leaves an operator renaming the
other and wondering why nothing changed.

Files are read **sorted by stem**. Roster order is load-bearing — a company that
tags nobody `tier = "orchestrator"` gets its first-listed teammate — and
readdir order varies by filesystem, so an unsorted read would make which agent
runs the company depend on which machine parsed the bundle. A company that
relied on declaration order under the inline form MUST state
`tier = "orchestrator"` when moving to the per-file form.

Only the immediate directory is read. `agents/prompts/` holds documents, not
teammates.

## Schema

Every key below is available in **both** forms — one type, one validator, one
consumer, so adopting a custom prompt does not require adopting the bundle
layout first.

```toml
# agents/copywriter.toml
role = "Copywriter"                     # required
description = "Write ads and campaign copy."
tier = "reasoning"                      # cognition hint; never selects a model
harness = "deep"                        # which [[harness]] runs this agent's
                                        # turns — see harnesses.md. Omitted
                                        # means the company's default harness.
provider = "anthropic"                  # this agent's own {provider, model}
model = "claude-sonnet-5"               # pair — see below. Omit both to
                                        # follow the company default.
tools = ["docs.*", "mcp:notion"]        # grant globs — see tools.md
delegates_to = ["creative"]             # narrow hand-offs to these desks (omit = anywhere)
budget_usd_daily = 5.0                  # per-agent daily cap

prompt = """                            # appended to the generated persona
Write for the reader, not the client.
"""
prompt_files = ["prompts/house-style.md"]   # checked-in, bundle-relative
context = [                                 # live workspace documents
    "brand/brand-voice.md",                 #   read only (the bare-string shorthand)
    { path = "agents/copywriter/drafts", access = "write" },  # + workspace_write/workspace_create
]
classes = ["evidence"]                      # routing exclusions — see below

ledgers = [                                 # per-agent ledger access (omit for unrestricted)
    { name = "tasks", access = "record" },
    { name = "decisions", access = "read" },
]
can_declare_ledgers = false                 # may this agent `define_ledger`? default true
```

### `tier` versus `harness`

They answer different questions and are deliberately separate fields.

`tier` names a **workload** (`reasoning`, `vision`, …) and is resolved against
whatever provider the agent's harness turns out to use. `harness` names the
**engine and the credential**. So an agent keeps its tier when it moves between
harnesses, and two agents sharing a tier on different harnesses run on different
models — which is the point of naming more than one.

Naming a harness the company does not declare is a validation error, reported
against both the agent and the id. Naming none is not: every roster written
before `[[harness]]` existed binds nobody, and all of them keep working.

### `provider` and `model`: the agent pair

On a `built_in` harness, `provider` and `model` together are this agent's own
resolved endpoint (keys rework, issue #2306) — a slug in the company's
`inference/providers` console list, and the model id that provider serves.
Set together or not at all: `provider` alone or `model` alone is a validation
error. Neither set means this agent follows the company default, resolved the
same way a turn with no pin does.

On an `acp` harness `model` keeps its older, unrelated meaning — the model
hint forwarded to that coding CLI's own session (see `[harness.acp].model`
in harnesses.md) — and `provider` is refused outright: an ACP agent brings
its own credential, so naming a console provider is meaningless.

The manifest cannot see the company's console-side provider list, so a
`provider` slug is checked only for shape at load — never for existence. A
typo (`provider = "antropic"`) loads clean and fails the agent's *first
turn* instead, with a message naming the agent and the fix: `resolve`'s pin
check refuses before falling back to the default. There is no fallback: a
pinned agent whose provider is removed or switched off is not silently
served by the company default.

Resolution order for a `built_in` agent's own turns: this pair, then the
harness's own `[harness.inference]` (see below), then the company default,
then an actionable refusal.

### `context` write access

A bare string in `context` is read-only — routed into the prompt, nothing
more. `{ path, access = "write" }` additionally puts that exact path in this
agent's `workspace_write`/`workspace_create` scope.

**Omitting every write entry is unconfined**, matching every manifest written
before this existed: `workspace_write`/`workspace_create` reach anywhere in the
company's tree, as they always could. Declaring **at least one** write entry
confines this agent's `workspace_write`/`workspace_create` to exactly the paths
it declared, plus its own `agents/<id>/` home, which stays writable regardless
— a role narrowed to a real access list must not also lose the ability to
produce and revise its own work. See `src/harness/workspace_tools.rs` for the
enforcement and why the pre-existing unconfined default is otherwise
unchanged.

### `ledgers`

Which of the company's ledgers this agent's five ledger tools
(`list_ledgers`, `read_ledger`, `record_entry`, `close_entry`,
`define_ledger`) can see and use, and at what access.

An omitted `ledgers` key is **unrestricted** — every ledger, at `record`
access — the tool surface every agent had before this field existed. A
declared list restricts `list_ledgers`/`read_ledger` to exactly the slugs
named (an undeclared slug is invisible, not merely unwritable), and
`record_entry`/`close_entry` additionally require `access = "record"` on that
entry. A bare `{ name = "tasks" }` with no `access` key defaults to `read` —
the safer of the two.

This is the **visibility and read/record** half of ledger access; a ledger's
own `writers` list (`docs/spec/runtime/ledgers.md`) stays the authoritative
check for whether a write actually lands. Declaring `access = "record"` for a
built-in ledger whose `writers` excludes this agent is a manifest validation
error — the two must not silently disagree. A company-declared ledger is not
cross-checked at manifest-load time, since it may not exist yet; any
disagreement there is an ordinary tool refusal at call time.

`can_declare_ledgers` (default `true`) governs `define_ledger` alone — a
company discovers which axes it needs while running, so declaring one is
unrestricted by default; set it `false` to keep a narrow role from growing the
registry.

## The prompt

An agent's system prompt is assembled in this order, and the order is a decision:

1. the generated **persona** — who this teammate is, at which company;
2. its inline **`prompt`**;
3. its **`prompt_files`** bodies;
4. its **team** — the roster, the desks it sits on and who else does, and
   the desks a referral from it may reach;
5. tool briefs (workspace, ledgers, sandbox, publishing, skills catalogue,
   and the hand-off brief);
6. its routed **`context`** documents.

Static material first, volatile last. The prompt prefix is what a provider cache
reuses across turns, so a workspace note the operator edits between two turns
must not invalidate the briefing behind it.

### The team section

Step 4 (`company::team_brief::team_section`) tells every agent who else is at
the company: each other roster teammate by id, role and mandate (the
orchestrator marked as such), each desk with its members and lead, the desks
this agent sits on, and — only when its `delegates_to` narrows it — exactly
which desks a question from it may cross to, rendered from the same rule the
referral policy enforces so the prompt never names a desk a referral would
refuse. A roster of one gets no section.

It exists because an agent that is not told it has colleagues does not use
them. On a desk, colleagues are reached by **speaking**: `post` to the room,
`dm` to named seats, `broadcast` when the room should decide who picks it up
([hive.md](hive.md#speaking)); another desk is reached by a referral
([hive.md](hive.md#referral)). Every roster agent also carries `spawn_task`,
and the brief under it (`orchestrator::member_delegation_brief`) says when to
leave a slice tracked on the board rather than in the conversation. The
orchestrator gets the same section ahead of its own brief, so it can assign by
id without a `query_company` call first.

### The board is a tool call

Nothing said in chat becomes a card on its own. A message typed into a desk
or a DM used to be carded by construction — the REST handler opened one for
anything that led with an action verb, and the runtime opened one for anything
"substantial" said to a desk lead — so every message became a work item nobody
had asked for and the answering agent had no say. Both paths are gone. A card
exists because an agent called `spawn_task`, because the orchestrator called
`assign_task`, or because a person opened one from the console or pressed the composer's
"Build me the workflow" control. The lexical triage (`company::task_intent`)
still runs, but only to narrow the model's board tools on a question and to
take the cheap chat-only path on a greeting.

### The sandbox brief

Step 4 includes a **sandbox** brief (`harness::toolbelt::sandbox_brief`) naming
the agent's own working directory and the tools that reach it: `file_read` /
`file_write` / `edit` / `list` / `glob` / `grep` under the `files`/`docs` grant,
`shell` and `read_workspace_state` under `shell`, and `apply_patch` /
`git_operations` / `csv_export` under `code`. Each clause is emitted only under
the grant that wired its tools, and an agent holding none of the three gets no
section.

It is not cosmetic. A granted tool the prompt never names is, in practice, a
tool the agent does not use: asked to *write* something, an agent that had never
been told it holds `file_write` recorded a task about writing it instead, and
`shell` — wired since the exec cell — was named in no brief at all. The brief
also states the path confinement the belt already enforces for the file/code
tools (`exec_security` sets `workspace_only`, so an absolute path or a `../`
escape is refused), because an agent that does not know it spends turns
rediscovering it one refusal at a time. The shell clause is deliberately not
framed that way: `action_dir` only sets the command's working directory, and a
same-uid command can read anywhere the server can
([agent-isolation.md](../security/agent-isolation.md)), so the brief describes
the directory as where shell commands *start*, never as a jail.

The `shell` clause tracks what was **wired**, not what was granted:
`toolbelt::shell_tools` withholds the whole namespace when the per-agent audit
logger cannot initialize, and the brief follows it rather than the grant.

The `shell`/`code` clauses also track the per-turn capability tier
(`capability_budget::resolve_filter`, live at `HarnessPool::ensure`): a fail-closed
metering error or an exhausted budget makes `filter_by_capabilities` drop the
matching tools from the vector `build_agent` hands to the builder, and
`sandbox_brief_flags` withholds the matching clause for the same turn — so the
brief never instructs an agent to call a tool the filter has already removed.

Step 5 is resolved by the async caller before the (synchronous) agent build and
fingerprinted over document **bodies**, so editing a routed note reaches the next
turn rather than the next restart. See
[context-routing.md](orchestration/context-routing.md).

**A named teammate is told its name.** A manifest `[[agent]]` is addressed by its
role, and its persona reads *"You are the Content Writer at Acme."* An
operator-added teammate also has a display name — the one the console puts on the
DM header, the subtitle and the composer — and its persona names it too: *"You are
Alex, the Content Writer at Acme. … Teammates and the operator address you as
Alex; it is how you are called here, not a separate character to play."* The name
is an addressing handle, not an identity to build a character around, and it never
replaces the role. A name that is blank, or that only restates the role, falls
back to the role-only wording (issue #1105).

**`role` is required on every path that can create an agent**, including the
console's (issue #1989). The line above interpolates it **unguarded**, unlike
the description and instructions blocks beside it, so a blank one ships the
teammate a persona reading *"You are Dana, the  at Acme."* and gives the
orchestrator's Team block `id — ` to delegate on — neither of which errors and
neither of which anybody is told about. `company.toml`, `agents/<id>.toml`,
the orchestrator's `add_agent` tool and both console write routes all refuse a
blank or whitespace-only role; `POST …/team` was the last one that did not.

**`prompt` is appended, never substituted.** The generated line is what binds the
agent to *this* role at *this* company; a prompt that replaced it would silently
cost the agent its identity and hand it back the runtime's own assistant
persona. What belongs in `prompt` is how the role works, not who it is.

### `prompt_files` versus `context`

They are the static and dynamic halves of the same idea, and they differ on
exactly one rule that matters:

| | `prompt_files` | `context` |
| --- | --- | --- |
| Source | the company bundle, under `agents/` | the live workspace tree |
| Read | once, at manifest load | on every roster rebuild |
| Missing file | **validation error** | skipped |
| Position | early (cache-stable) | last (volatile) |

The missing-file split is deliberate. A `context` entry names operator-owned
live state that may legitimately not exist yet. A `prompt_files` entry names a
file in the same commit as the agent referencing it, so a typo there yields a
role whose prompt was written around a briefing it silently never received —
which fails confidently rather than visibly.

A `prompt_files` path may not escape `agents/`. The check is on path components,
before touching the filesystem, rather than by canonicalizing: canonical
comparison resolves symlinks, and whether a bundle is valid must not depend on
how the checkout was laid out on the reading machine.

### Seeing the assembled prompt

A brief is the most editable thing in a bundle and used to be the least
inspectable: reading one as the agent receives it meant running the company and
reading a provider trace. `opencompany prompt` renders the same composition from
a manifest alone.

```sh
./scripts/dump-prompt.sh --company companies/product_team
./scripts/dump-prompt.sh --company <dir> --agent bug_triager       # one teammate
./scripts/dump-prompt.sh --company <dir> --agent bug_triager --raw # bytes only
./scripts/dump-prompt.sh --company <dir> --out /tmp/prompts        # a file each
./scripts/dump-prompt.sh --company <dir> --json                    # machine-readable
```

The report names every section, where its bytes came from (a manifest field, a
bundle file, a brief function), and the agent's effective grants — a missing
brief is almost always a missing grant. `--raw` prints the concatenation exactly
as the harness performs it, which is what makes it diffable against a real
trace.

**What it cannot render, it names.** Routed `context` bodies need a live
workspace store, the skill catalogue needs a materialized bundle directory, the
MCP brief needs a configured registry, and the vendored runtime's safety
preamble and grounding suffix need a live `PromptContext`. Each appears under
*Not rendered here* with the reason, so a section missing from the dump is
visibly missing rather than invisibly absent.

The wrapper exists for the feature flag: the harness owns the tool briefs and
compiles only under `--features openhuman`, so calling the subcommand from a
default build produces a shorter prompt that would otherwise look complete.
Composition itself (`src/company/prompt_dump.rs`) is always compiled — a
debugging surface that only exists in a feature build is one nobody runs.

### Budgets

Each document section is clamped to `PROMPT_FILE_BUDGET_CHARS` (10,000
codepoints, a tokenizer-free upper bound on the brief budget in
[alignment.md](orchestration/alignment.md)). The clamp keeps the **leading**
portion, cuts on a character boundary, and appends a visible marker.

The budget applies to the **section**, not per document: a role routed five
documents and a role routed one spend from the same prompt. Clamping happens at
assembly, where the text is spent — refusing the read would cost the company the
whole document, while clamping the tail costs only the tail.

An empty or whitespace-only document is dropped rather than rendered as a bare
heading. An empty section reads to the model as a source that exists and says
nothing, which is worse than its absence.

## What a turn is allowed to spend

Moved to [agents-turn-limits.md](agents-turn-limits.md): the 25-round tool
iteration cap and the reasoning behind the number, the in-turn spend brake armed
only for a teammate with a declared daily budget, and why a cap pause and a
budget halt are reported separately.

## `classes`

The explicit epistemic classification
[context-routing.md](orchestration/context-routing.md) requires. Three values,
each subtracting one document:

| Class | Excludes | Prevents |
| --- | --- | --- |
| `evidence` | the assertion board | a role weighing evidence scoring an unevidenced sentence beside a real one |
| `judge` | the scratch | provisional working-out read as progress, which keeps a loop retrying |
| `directive` | the claim ledger | a role carrying out an instruction filing that instruction as a finding |

Declaring none is *unclassified*, which imposes no exclusion and is the right
default: an ordinary teammate is not judging anything.

An exclusion **outranks** both the tier default and an explicit `context` list.
That is what makes a declared class a control rather than a suggestion someone
can edit away. The universal method document is exempt — it is method, not
assertion, and a role excluded from it could not follow it.

The classification MUST be declared, never inferred from `role`: `role` is prose
an operator writes for humans, so matching on it would make a company that
renames "Critic" to "Reviewer" silently lose an exclusion, and a control a
rename can switch off is not a control.

## Where this lives

| Concern | File |
| --- | --- |
| Bundle loading, `prompt_files` resolution | `src/company/agent_file.rs` |
| Prompt composition and clamping | `src/company/prompt.rs` |
| Rendering a composed prompt back out (`opencompany prompt`) | `src/company/prompt_dump.rs` |
| Routing table and exclusions | `src/company/context_routing.rs` |
| Roster type and constants | `src/company/types.rs` |
| Manifest wiring and validation | `src/company/manifest.rs` |
| Iteration cap, stated on every built agent | `src/harness/build.rs` |
| In-turn spend brake, installed per turn | `src/harness/mod.rs` |

The first three are **always compiled**, though the harness that spends the
prompt is behind the `openhuman` feature. Composition, clamping and the
exclusion table are pure decisions with real edge cases, and the exclusions are
controls — they deserve tests in every build, not only where the agent runtime
links.
