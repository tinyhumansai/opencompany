# What already works — read this before designing anything new

Every claim below was verified by reading the cited file directly during
this brief's research pass (2026-09-18), not inferred from a doc.

## 1. An agent can already bind to a bare, undeclared CLI harness

`Agent.harness: Option<String>` — `crates/opencompany-core/src/company/types.rs:702`:

> "Which `[[harness]]` this agent runs its turns on, by id. `None` means the
> harness marked `default = true`."

Nothing about this field requires the id to be declared in `company.toml`.
That's deliberate — see point 2.

`Agent.model: Option<String>` (same file, line 713) is a model hint forwarded
to the agent's ACP harness (on an `acp` harness) or the model half of a
`{provider, model}` pair (on a `built_in` harness) — already supports the
per-agent override the design needs, no new field required there either.

## 2. A lane is synthesized on demand for any agent bound to a detected CLI

`harness/lanes.rs`'s `referenced_implicit_locals` (line 257):

```rust
fn referenced_implicit_locals(
    record: &CompanyRecord,
    declared: &[Harness],
    default_harness: &str,
) -> Vec<String> {
    let effective = record.effective_agents();
    let manifest_bindings = effective.iter().filter_map(|a| a.harness.as_deref());
    let overlay_bindings = record.overlay_agents.iter().filter_map(|a| a.harness.as_deref());
    // ...filters to ids that are implicit-local, not already declared, not the default...
}
```

Reads **both** roster halves — manifest agents and console-added overlay
agents — specifically so a console-added teammate bound to a detected CLI
gets a lane too. `build()` (line 298) calls this and, for each id it
returns, calls `Harness::implicit_local(&id)` then `resolve_acp_engine` to
either produce a real lane or record why it can't (`unavailable`).

This is **on-demand by design**, not an oversight — the module doc explains
why (lines 20–37): synthesizing a lane for every known CLI on every company
would take every company off the fast path
(`HarnessBrain::run_turn` returns the plain engine only when `lanes` *and*
`unavailable` are both empty). A company that binds nobody to a CLI adds
nothing here.

## 3. A binding that can't run fails cleanly — never falls back silently

`lanes.rs`'s own module doc (lines 20–27) states the doctrine directly:

> "Rather than silently routing those agents somewhere else, the harness is
> recorded as unavailable with the reason, and a turn bound to it fails
> saying so. Falling back would be the worst outcome available — the turn
> would succeed, on a model and a credential nobody chose."

`resolve_acp_engine` (line 86) is where this actually happens: it calls
`factory.build(agent_id, acp.model.as_deref(), agent_models, workspace_root)`
(line 113–114) — the desktop shell's live `AcpAgentFactory` — and on
failure, `.map_err(|error| format!("\`{agent_id}\` could not be started:
{error}"))` (line 119) becomes the `unavailable` entry's reason string. This
is the exact mechanism the corrected flow's Step 4 (turn-time failure) needs
— it already exists, already runs, already produces a specific reason rather
than a generic error.

`unavailable_reason` (line 73) also already distinguishes the build-level
case cleanly:

```rust
"acp" => "it is an ACP harness and this build has no ACP transport wired — \
          run it from the desktop app, or bind these agents to a `built_in` harness"
```

## 4. Hosted (non-desktop) builds are already prevented from offering this

`app/types.rs:809`, `AppState::can_run_local_acp`:

```rust
pub fn can_run_local_acp(&self) -> bool {
    cfg!(feature = "acp") && self.acp_agents.is_some()
}
```

The doc comment directly above it (lines ~795–808) describes the exact
failure mode this was built to close, worth quoting because it's precisely
the bug class this brief's design has to avoid reintroducing:

> "The result was a picker that offered `claude` and `codex`, a `PATCH` that
> accepted the binding, and then every turn failing with `lanes.rs`'s 'run it
> from the desktop app' — advice for somebody already in it."

`server/ops/harnesses.rs:143-146`'s `GET {scope}/harnesses` gates the
synthesized `detected: true` rows on this predicate — a hosted/server build
never offers a CLI harness as pickable in the first place. This is already
correct and needs no change; the new work must not duplicate this gate with
a second, possibly-drifting copy of the same check (the doc comment
explicitly names the earlier drift-caused bug: "the same conjunct at both
call sites... a predicate the two can state differently is what opened the
gap in the first place." **Reuse `can_run_local_acp()`, or the harness list
it already gates, for anything new — never re-derive it.**

## 5. The agent-level picker UI already exists and already works — including model pinning, not just harness binding

`frontend/src/views/team/AgentDetailView.tsx` — the "Harness & Model editor"
(explicitly commented as "issue #1245's harness-picker follow-up",
line ~313). It:

- fetches the company's harness list via `client.listHarnesses(company)`
  (line ~499) — the same `GET {scope}/harnesses` from point 4, so it already
  only shows CLI options on a build that can run them;
- lets an operator pick any returned harness id, including a synthesized
  `detected: true` one;
- saves via a single `PATCH` (`saveHarnessAndModel`, line ~670) setting
  `edits.harness` **and, when applicable, `edits.model` in the same
  request** (line ~683-684: `if (model !== undefined) edits.model = model;`)
  — reusing `harnessEdit()`'s and `modelEdit()`'s existing `""`-means-default
  contracts.

**This already works, today, in the running app, model selection included —
not just the harness toggle.** An earlier pass through this brief's own
research (and a first correction attempt) assumed model pinning needed new
UI, based on `local_agent.rs`'s backend steering mechanism alone, without
checking whether the frontend already exposed it. It does, fully:

- The `HarnessAndModel` component (`AgentDetailView.tsx:1806` onward) renders
  a second `<Select>` for the model, shown only when the drafted harness is
  `acp` (`draftKind === "acp"`, line ~1953), directly below the harness
  picker in the same "Harness & model" editable section — not a separate
  dialog, not a company-level setting.
- The model list is **already live-refreshed**, not static: `ensureAcpModels`/
  `cachedAcpModels` (line ~1878-1891) paint a cached list instantly, then
  replace it with a fresh probe result the moment the editor opens on an ACP
  harness or the drafted harness changes — the same live mechanism point 6
  below found missing from the *harness* row's readiness badge already
  exists for the *model* list within it.
- Cross-harness validity is handled correctly: switching from `claude` to
  `codex` clears a model pin that no longer applies (`acpModelStillValid`,
  line ~1214, compared on the CLI's own `agent` id since two harness ids can
  drive the same CLI), and switching away from `acp` entirely drops the pin
  rather than silently sending a value the new harness kind would refuse
  (`crossedPairBoundary`, line ~1222).
- A pin the harness no longer advertises (a model retired, or a stale
  company config) is still shown rather than silently dropped
  (`unlistedModel`, line ~1893-1894) — the exact "stale detected-model list"
  edge case worth checking for is already handled, not a gap.

**Not to be confused with `DefaultModelDialog`** (`frontend/src/inference/
DefaultModelDialog.tsx`, used by `ProvidersTab.tsx` on the company's
LLM/Provider page). That is a genuinely different feature for a genuinely
different scope: setting the **company-wide default** `{provider, model}`
pair a teammate falls back to when it has no pin of its own, with its own
replace-confirm semantics (its own doc comment: "there is no separate
'clear the default' action... it always requires a model... a default is
never a provider alone"). `HarnessAndModel`'s inline model `<Select>` is a
**per-agent override**, a different mechanism with different semantics, and
does not need to adopt that dialog's pattern — doing so would mean building
new UI to replace something that already works correctly, the exact
anti-pattern this whole brief exists to avoid.

**This already works, today, in the running app.** It is not a mockup, not a
half-built stub — it's the real, shipped mechanism for exactly what this
brief set out to build.

## 6. The one thing genuinely missing from that picker: no live readiness

`harnessOptionLabel` — `frontend/src/lib/agent.ts:408`:

```rust
export function harnessOptionLabel(harness: HarnessDto): string {
  if (harness.kind === "acp") {
    const cli = harness.agent ?? "external";
    return harness.transport === "runner" ? `${cli} (remote) — ${harness.id}` : `${cli} — ${harness.id}`;
  }
  return `Managed — ${harness.id}`;
}
```

Builds the picker's label from `HarnessDto` alone — `kind`, `agent`,
`transport`, `id`. **No readiness field anywhere in this function or its
input type.** An operator sees "claude — claude" whether Claude Code is
signed in, not installed, or doesn't exist on this machine at all. This is
the actual, narrow, real gap — see `02-the-real-gap-and-fix.md`.

Notably, the readiness typing already exists and is already shareable:
`frontend/src/lib/harnesses.ts`'s `HarnessRow` carries an optional
`readiness?: AcpReadiness` field (line 52), computed by `joinHarnesses()` and
consumed today only by the standalone `external-harnesses.tsx` page. Reusing
this exact type and its helpers (`isUsableHere`, `isChecking`,
`readinessNote`) in the agent picker is additive frontend wiring, not new
backend surface.
