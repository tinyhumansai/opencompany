# In-use guards: the wire shape all three agents share

Part of [the keys rework](README.md) (issue #2306). This is the shared
contract for "you are about to remove, clear, disable or switch something
other config depends on" across Agent A's scope (Composio, **search**,
account key), Agent B's scope (provider rows, the default, agent pairs — this
file's primary author), and Agent C's console. Written first and pushed early
so all three agents code against the same shape without asking.

**Revision history:** rewritten 2026-09-15 after round-1 review found the
first draft contradicted decision X14 (§4), scattered the turn-failure
sentences instead of sharing one table (§5), misattributed search ownership
(§1), left `surfaces` underspecified (§2), listed key-rotate as a guarded
action in error (§2), and undercounted agent pairs (§6). This version fixes
all six; nothing here should be read against the first draft.

## 1. `usedBy` on DTOs

Any DTO for a thing another piece of config can depend on carries an optional
`usedBy` field:

```ts
usedBy?: {
  default?: true;               // this is the company's inference default,
                                 // or the search default (search/default)
  agents?: { id: string; name: string }[]; // agent pairs naming this provider
  surfaces?: ("llm" | "composio" | "search")[]; // which product surfaces use it
}
```

Rules:

- Every sub-field is omitted when empty (`skip_serializing_if` on the Rust
  side; no empty arrays or `false` on the wire) — see `src/error.rs`'s
  `UsedBy`/`UsedByAgent`/`UsedBySurface`, a typed struct, not a raw
  `serde_json::Value`.
- The whole `usedBy` field is omitted (not `null`, not `{}`) when nothing uses
  the thing, so `"usedBy" in dto` is itself the in-use check on the console.
- `agents` lists every agent whose pair (§6) names this provider's slug, by id
  and display name, in roster order.
- `default` is `true` iff the relevant default (LLM: `store::load_default`;
  search: `search/default`) is set to this slug — `Full` or `ProviderOnly` for
  LLM, the bare stored slug for search. A bare-slug default counts:
  switching that provider off or deleting it still needs confirmation,
  because the operator's stated intent is "this is my provider."

**Who owns which `usedBy` producer:**

| Producer | Owner | Notes |
|---|---|---|
| `ProviderDto.usedBy` on `GET .../inference` and `.../inference/providers` rows | **B** | `default` from `load_default()`; `agents` from every pair naming the row's slug (§6) |
| A provider **key** clear/disable (`POST .../providers/{slug}/enabled` off; a key clear) | **B** | Same `usedBy` the row's status shows — disabling or clearing the key strands the same dependents a delete would |
| Composio token/BYOK key clears | **A** | `surfaces` includes `"composio"` whenever a workload still resolves through it |
| The account key (`tinyhumans/key`) clear/disable | **A** | See §2's `surfaces` table below — `"llm"` appears only when a `tinyhumans` provider row exists (D-set/X5) |
| **Search** provider removal, disable, and the search default | **A** | Corrected from the first draft, which wrongly filed this under C and wrongly guessed search had no default. Search **does** have a default (`search/default`) and **does** get confirm dialogs on removal/disable, on the same `usedBy`/`confirmInUse`/409 contract as everything else here. X14 (§4) extends the same way: disabling or deleting the search default's provider never clears `search/default` — A implements this; kept here so the two never drift. |
| Console dialogs (LLM, Composio, search) | **C** for LLM/Composio pages, **A** for the search page it owns | A confirm dialog on every destructive action and toggle; `usedBy` rendered in it; a 409 reopens the dialog with fresh `usedBy` |
| Set-default (`POST …/inference/providers/{slug}/default`) | **B** | **Not a `usedBy` producer and not a 409 case** (round-3a review P2-6) — see §2. Nothing depending on the provider being replaced as default loses anything; the console's own confirm dialog, naming the old and new provider, is the whole guard. |

## 2. Refusal without confirmation

Any mutation that **removes, clears, disables, or switches** something with a
non-empty `usedBy` is refused unless the request carries `confirmInUse:
true`:

- in the JSON body for POST/PUT/PATCH;
- as the query parameter `?confirmInUse=true` for DELETE (which has no body
  on this API).

**A key rotate/replace (a save that writes a new, non-blank value over an
existing one) is never guarded.** Only removing, clearing, disabling or
switching something is destructive to a dependent; a rotate keeps serving
every dependent it already had, just with a different credential behind it.
The first draft of this file did not list rotate as guarded, but said it
imprecisely enough that a reader could infer it was — this line removes that
reading.

**Set-default (`POST …/inference/providers/{slug}/default`) is never guarded
either, and it is not a 409 case** (round-3a review P2-6, decided
2026-09-15). Unlike a delete, disable or key clear, set-default strands
nothing: the provider being *replaced* as the default keeps existing, keeps
its key, and keeps serving every agent pair that names it directly — only the
*unpinned* traffic moves, and moving unpinned traffic to a different provider
is the one thing this route exists to do. X4 already puts the safeguard on
the client: the console's confirm dialog names the old and the new provider
before the request is sent, and that confirmation — not a server-side
`usedBy`/`confirmInUse`/409 round trip — is the guard. `SetDefault` therefore
carries no `confirmInUse` field and `provider_used_by` is never called on
this path.

**Refusal is HTTP 409**, in the repo's existing API error envelope. That
envelope is `ApiError` (`src/server/error.rs`), built from
`crate::error::OpenCompanyError` (`src/error.rs`). Every error renders as
`{"error": "<sentence>", "code": "<stable_snake_case>"}` — `error` is the
`Display` string, `code` is `OpenCompanyError::code()`. One variant,
`WorkflowInvalid`, additively carries one extra top-level key (`problems`) by
special-casing the match arm in `ApiError`'s `IntoResponse` impl
(`src/server/error.rs`); nothing else does. Do **not** invent a second
envelope. The keys-rework code follows that exact precedent: one new variant,

```rust
OpenCompanyError::InUse { message: String, used_by: UsedBy }
```

mapped to `StatusCode::CONFLICT` in `ApiError::status()`, `"in_use"` in
`OpenCompanyError::code()`, and one more special-cased arm in `IntoResponse`
that adds `"usedBy": used_by` alongside `error` and `code`. `UsedBy` (defined
in `src/error.rs`, beside `InUse`) is a **typed struct** — `default: bool`
(`skip_serializing_if` when `false`), `agents: Vec<UsedByAgent>`,
`surfaces: Vec<UsedBySurface>` (`UsedBySurface` an enum of the three strings,
`#[serde(rename_all = "lowercase")]`) — not a raw `serde_json::Value`: a typed
shape is what every caller in this repo can construct without restating the
wire spelling, and what a client can rely on structurally.

```json
{
  "error": "Anthropic is used by the company default and 2 agents: Researcher, Web search.",
  "code": "in_use",
  "usedBy": { "default": true, "agents": [{"id": "researcher", "name": "Researcher"}, {"id": "web_search", "name": "Web search"}] }
}
```

The message names **every** user, in one sentence:

- `default` alone: `"<Label> is the company default."`
- `default` + N agents: `"<Label> is used by the company default and N agent(s): <names, comma-joined>."`
- agents only: `"<Label> is used by N agent(s): <names, comma-joined>."`
- a key clear/disable with only `surfaces`:
  `"<Label>'s key is used by <surfaces, comma-joined>."`

`N agent(s)` is `"1 agent"` or singular-free `"N agents"` — never
`"1 agents"`.

**Accepted variant (P3-2, keys rework #2306 review):** the account-key
clear's own message (`account_key_in_use_message`,
`crate::company::company_key::fan_out`) does not follow the generic
`surfaces`-only template above. It reads `"Used by TinyHumans on the LLM page
and by Composio."` (or, with one surface, `"Used by Composio."`) rather than
`"TinyHumans account key's key is used by llm, composio."` — an accepted
operator decision (2026-09-15 review): the account key has no single
`<Label>` a reader would recognize the way a provider's display name reads,
and naming the surfaces themselves in plain words reads more naturally for a
one- or two-item list than forcing the raw template's comma join. Every other
guard in this file (a provider row, a key, Composio's own guard) still uses
the generic template unchanged; this is the one deliberate, documented
exception.

### What each `surfaces` entry means, precisely (P2 fix)

`surfaces` is populated **per credential/row being guarded**, not globally —
each entry says a specific product path still resolves through the specific
thing this mutation would touch:

| Surface | Appears when | Owner |
|---|---|---|
| `"llm"` | The mutation touches `provider/tinyhumans/key` (or another provider's key/row) **and a matching row exists in `inference/providers`** — D-set (and its X5 sharpening): a key with no row is not "set", so it can never make `"llm"` appear. Concretely: the account-key guard adds `"llm"` only when a `tinyhumans` row already exists; a company that has only ever saved the account key (no row) never sees `"llm"` on that guard, because nothing in the LLM surface is actually depending on it yet. | A (account key), B (a provider row's own key) |
| `"composio"` | The mutation touches `composio/tinyhumans/key` or `composio/byok/key` and `composio/mode` currently selects that slot, i.e. a workload would actually lose tool access | A |
| `"search"` | The mutation touches a search provider's key/row and `search/default` (or an agent path, if one is ever added) resolves through it | A |

A single mutation (e.g. clearing the account key, which fans out to
`tinyhumans/key`, `composio/tinyhumans/key`, and — only if a row exists —
`provider/tinyhumans/key`) can therefore report more than one surface at
once; that is the intended shape, not an edge case to collapse.

## 3. With confirmation

`confirmInUse: true` (or `?confirmInUse=true`) makes the mutation proceed
exactly as it would with no dependents, and the response **echoes** `usedBy`
(the same shape, computed **before** the mutation applied) so the caller's UI
can show what it just broke. `usedBy` on a success response uses the same
optional-field-omission rule as §1.

## 4. Q8 / D-never-clear-default (X14): confirmed removal keeps the default

A confirmed removal of a provider's key, a confirmed disable, or a confirmed
**delete** of a provider row — while that provider is the company default (or
an agent pair), or, for search, the search default — **never rewrites
`inference/default`, an agent's pair, or `search/default`**. The stored
default (or pair) keeps naming that now-missing or now-disabled provider.
This matches decision Q8's TinyHumans case, generalized to every provider (LLM
and search alike) by decision D-never-clear-default (X14, 2026-09-15,
`docs/key-reworks/README.md`): the operator asked for the removal and
confirmed it; a silent rewrite would be a second undocumented decision the
operator did not make. What happens next is the turn-time fail-closed
behaviour in §5 (or, for search, the existing "nothing answers" reporting),
and status exposes that the default/pair points at a missing or disabled
provider so the console can show a banner instead of conjuring a new default.

**No carve-out for delete, disable, or key-clear — none of the three ever
clears a marker.** The first draft of this file kept a carve-out where
*deleting* a provider row (as opposed to merely disabling it) still cleared
the default, reasoning that "the row is gone, not just disabled" made it a
different case. Round-1 review correctly called this a direct contradiction
of X14, which draws no such line: X14's rule is about the **default/pair
marker**, and a marker naming a deleted slug is exactly the state X14 asks
for, on the same footing as a marker naming a disabled one. Concretely, in
`src/server/ops/inference/providers.rs`:

- `delete_provider` **no longer calls** `clear_default_if_marked` (removed
  from the site that used to sit right after the credential/health cleanup).
- `set_enabled(..., false)` **no longer calls** `clear_default_if_marked`
  either.
- `clear_default_if_marked` itself is deleted once both call sites are gone
  (nothing else calls it); if a later slice finds a reason to keep the
  function for another purpose, that is a new decision, not a revival of this
  one.
- `get_provider` on a deleted or disabled slug still answers `None`/disabled
  exactly as before, so `resolve_choice`'s fail-closed turn error (§5) is
  unaffected — only the **stored marker** stops being silently rewritten out
  from under the operator.
- Tests added alongside the removal: the default (and an agent pair) survive
  a delete; survive a disable; survive a key clear; and the status DTO
  reports the dangling default/pair (via `defaultChoice` continuing to name
  the now-gone slug, plus the F6 turn error) rather than silently reporting
  "unset".

Search gets the identical treatment in Agent A's slices: disabling or
deleting the provider `search/default` names never clears that key either —
`DELETE …/search/providers/{slug}`, `PUT …/search/providers/{slug}` (disable)
and `PUT …/search/providers/{slug}/key` (clear) all guard exactly as §1/§2
describe, and `DELETE …/search/key` (disconnect-all) carries the same guard
in bulk: `usedBy` is `{ "default": true }` whenever `search/default` names
*any* provider, refused without `?confirmInUse=true`, and a confirmed
disconnect-all never clears the marker either.

**Explicit carve-out: the legacy `PUT …/search` route.** Its own
"select managed" branch (`src/server/ops/search.rs`, `put_search`,
`provider == MANAGED_PROVIDER`) still clears `search/default` outright on
every call, unguarded and unconditional — the one place on this surface that
does not follow X14. This is intentional, not an oversight: the route
predates the indexed `search/providers` flow, has no console caller today
(`saveSearch` in `frontend/src/api/search.ts` is defined but unused —
confirmed by grepping `frontend/src` for callers), and is marked
`// DEPRECATED(keys-rework #2306)` at the site rather than rewritten to match
X14, because rewriting dead code's behavior "for consistency" is a change
with no observable effect and a nonzero chance of quietly breaking whatever
integration still holds the route by hand. A test in `search.rs` pins the
current (unchanged) behavior so a future removal of the route — the only
condition under which this carve-out goes away — is a deliberate decision
against a known baseline, not silent drift. If this route ever gains a real
caller again, the same guard-and-never-clear treatment above applies to it
with no further discussion needed.

## 5. Turn-time fail-closed messages (F6, D-copy / X9, D-names-in-errors / X7)

**One shared sentence table**, defined once in `src/company/inference/copy.rs`
(not written ad hoc per call site — this is the fix for round-1's finding
that the first draft scattered near-duplicate wording across
`phase-2b-default-shape-part2.md` and `phase-3a-agent-pair-backend.md`,
which are now both superseded by this file and by `copy.rs` itself). Every
function takes **display names** (X7: an agent's `role`/label, a provider's
`label`) — never a raw slug or agent id in the sentence a person reads; the id
still rides in whatever structured data the caller attaches (`AgentPin`
carries both).

The console path every sentence below names is **`Connections → API Keys →
LLM`** — verified against `frontend/src/views/connection-pages.ts`, where the
group id `keys` is labelled "API Keys" and the `inference` page inside it is
labelled "LLM". (The first draft said "Connections → LLM", skipping the group
label; round-1 review caught this as unverified against the actual console.)

| Situation | Function | Sentence |
|---|---|---|
| A provider save (add/edit/set-default) sent no model | `no_model_for_provider(provider_label)` | "Choose a model for {Provider} before saving." |
| An agent's turn: no pin, no full default, legacy chain gives nothing | `nothing_resolved(agent_name)` | "No model is chosen. Choose a provider and model for {Agent}, or set the company default in Connections → API Keys → LLM." |
| No agent in view (boot/status, or an internal pass with no single agent) and nothing resolves | `nothing_resolved_for_company()` | "No model is chosen for this company. Choose a default provider and model in Connections → API Keys → LLM." |
| A provider that would otherwise resolve has no credential | `provider_has_no_key(agent_name, provider_label)` | "{Agent} uses {Provider}, which has no key. Add one in Connections → API Keys → LLM, or choose another provider and model for {Agent}." |
| An agent's own pair names a provider that is gone or switched off (F6: fails closed, never falls back to the default on its own) | `pair_broken(agent_name, provider_label, why)` | "{Agent} uses {Provider}, which is removed. Choose another provider and model for {Agent}, or clear its model to use the company default." (or "which is turned off") |
| The company default names a provider that is gone or switched off | `default_broken(provider_label, why)` | "The company default uses {Provider}, which is removed. Choose a new default in Connections → API Keys → LLM." (or "which is turned off") |

`why: ProviderGone` is `Removed` or `TurnedOff` — the two ways a pin or
default can point at nothing servable, shared by `pair_broken` and
`default_broken` so the two wordings cannot drift apart on which word means
what. Every sentence above has a test in `copy.rs` asserting the exact text.

These are produced by `resolve_choice` (2b/3a) and the `TenantProvider::resolve`
pin check (3a). `resolve_choice` calls into `copy.rs`; the pin check's own two
sentences ("agent `{id}` is set to `{provider}`, which this company does not
have/is switched off…") are still written inline with the raw agent id and
provider identifier **in the sentence itself** — a known X7 gap (phase-3a's
gotcha G7), kept because `HarnessModel::pinned` carries only an id, not the
agent's display role, and fixing it needs a signature change out of this
slice's scope. Recorded here rather than silently left inconsistent with the
"display names only" rule the rest of this table follows.

### 5.1 The fail-closed chat notice, structured (KR-L2-03, 2026-09-15)

A turn that fails **because of resolution** (a pin naming a gone/off
provider, a broken company default, no model chosen, no key) is reported to
the chat UI as an ordinary `AgentReply`/history message, additively carrying
five more fields — present only for this one class of failure, so every other
turn error (a tool timeout, an empty response, a provider rate limit) is
unaffected and keeps today's generic "This turn couldn't be finished…"
wording with none of these fields set:

| Wire key | Type | Meaning |
|---|---|---|
| `userFacing` | `boolean` | `true` only for a classified resolution failure. Absent (reads as falsy) on every other failure and on every ordinary reply. |
| `code` | `string` | One of `no_model_chosen`, `pair_provider_removed`, `pair_provider_off`, `default_provider_removed`, `default_provider_off`, `provider_no_key`, `model_not_listed`, `harness_unavailable`. `model_not_listed` is reserved: no resolver check produces it yet (a chosen model is never re-validated against a live catalogue at turn time, by design — see phase-2b/2d), so no test exercises it either. `harness_unavailable` is produced by `HarnessRouter::engine_for`, not by a resolver: the turn never reached a model at all because the agent is bound to a harness this host has no engine for, or to one whose warm-up failed. |
| `message` | `string` | The exact `copy.rs` sentence (X9), with display names — identical to `text` on this same message for a classified failure. |
| `pairAgentId` | `string?` | The agent id this failure is about, when the resolver could name one **and had a raw id in hand** — `pair_provider_removed`/`pair_provider_off`, from `TenantProvider::resolve`'s own pin check (§5's G7 note), and `harness_unavailable`, whose sentence names the id itself. Absent for every other code, because `copy.rs`'s sentences are display-name-only (X7) and a display name is not a ref-able id. |
| `providerSlug` | `string?` | The provider slug or label the failure names, on the same terms as `pairAgentId` — a slug for the two pin codes, absent otherwise. |

**Not `agentId`.** The obvious wire name collides with `AgentReply.agent_id` /
the existing SSE `agentId` key, which is the reply's **author** — always the
system author for every failure notice, classified or not — and which other
code (and, presumably, the console) already keys "is this a system notice"
off. Overwriting it with a teammate's id for a classified failure would make
a fail-closed notice about `researcher` render as if `researcher` had sent
it. `pairAgentId` is additive and carries no such collision.

Backend: `resolution_failure(detail: &str)` in `src/server/operator.rs`
classifies the error text `turn_failure_notice` already receives, by fixed
substring (mirroring `provider_failure_sentence`'s existing pattern in the
same file) — there is no structured error type threaded through the
vendored turn loop to classify on instead. Applies to both the live SSE
`agent_reply` frame and the persisted `CompanyEvent::AgentReply` (so history
carries the same fields on reload, via `MessageView`/`ChatHistoryMessageDto`).

## 6. What counts as "used", precisely (Agent B's scope)

A provider row (by slug) is used by:

1. **The default** — `store::load_default(...)` is `Full(choice)` with
   `choice.provider == slug`, or `ProviderOnly(slug)`.
2. **An agent pair** — any of:
   - a manifest `[[agent]]` with `provider = "<slug>"`;
   - an `OverlayAgent` (console-added teammate) with `provider: Some(slug)`;
   - an `AgentOverride` (an edit on a manifest agent) with
     `provider: Some(slug)` (not `Some("")`, which is cleared).

   **Counted on the provider slug alone, whatever the model half holds**
   (round-1 review fix: the first draft required both `provider` and `model`
   to be non-blank before counting a pair as "using" the provider, which
   undercounts — an agent whose stored pair has a provider but a blank or
   missing model is still a record pointing at that provider slug, and a
   dependent-count that silently drops it would let a confirmed delete strand
   an agent the guard never warned about). Computed the same way
   `runtime/builder.rs`'s `agent_pairs` helper (5a's sibling, added in 3a)
   computes membership *for boot's `configured` check* — but note that helper
   has a narrower job (whether a pair can actually resolve a turn) than this
   guard's job (whether anything still *names* the slug), so the guard's own
   pass over manifest agents + overlay edits + overlay-only teammates filters
   only on `provider == Some(slug)`, not on `model` as well.

A provider **key** (credential) is used by the same two things, because a row
with no key cannot serve either dependent — clearing the key is functionally
equivalent to disabling the row from a dependent's point of view, so it gets
the same guard.

## 7. Routes are not part of this contract

`inference/routes` (until 5b removes it) is never a `usedBy` dependent and
never gates a guard: it is a legacy, unvalidated blob that 5a only ever reads
for a one-time carry-over, never as a live dependency. A provider named only
by a stale route can be deleted with no confirmation prompt.
