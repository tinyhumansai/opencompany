# Rebuilding a company runtime in place

Issue #290. Which brain a company runs is chosen **once**, in
`RuntimeBuilder::build`. A company that resolved no inference source at boot is
on the offline echo brain with an unwired workflow runner, and a credential
written afterwards reaches neither. #266 shipped the honest half of that: a
`restartRequired` flag on the inference status, a persistent console banner, and
a `restart_required` answer from `POST …/workflows/{id}/run`.

The restart is the problem. A hosted tenant's unit of restart is its container,
and the control plane has no button an operator can press: `POST
/api/v1/companies` refuses an existing id, archive removes a company with no
unarchive, and nothing else in the process ever replaced a registered runtime.
So first-time BYOK setup needed someone with pod access.

This is the seam that removes it.

## The sequence (`src/runtime/rebuild.rs`)

`runtime::rebuild_company(&state, id)`:

1. **Read the manifest** from the company's persisted record — before anything
   else, so a store failure can never leave a company parked. The record's
   manifest is the *materialized* one, which matters for two things a fresh
   `company.toml` read would drop: console-created workflows merged into
   `[workflows].enabled`, and the `[place]` fields `serve --discoverable`
   mutated before the original build.
2. **Quiesce** (`CompanyRuntime::quiesce`). Sets a flag that makes every cycle
   entry point refuse with `Quiescing` (`503`), then waits on the per-company
   `serial` lock, which a cycle holds for a whole turn. Both halves are
   necessary: the flag alone leaves a turn mid-cycle, and the wait alone races a
   cycle queued behind the one in flight.
3. **Hand over** (`CompanyRuntime::handover`). Snapshots the per-instance state a
   second runtime must not duplicate — see below.
4. **Rebuild** through the host's `RuntimeRebuilder`, which runs the *same*
   builder assembly boot ran (`company_builder` in `src/bin/opencompany.rs`).
5. **Swap** the successor into the registry.

## What must be inherited, not rebuilt (`src/runtime/handover.rs`)

Most of a runtime is `Arc<dyn Port>` handles that are safe to share. These are
not; a second copy of any of them is a correctness bug:

| Piece | Why a second copy breaks |
|---|---|
| `RuntimeJournal` | Single-writer. `append` writes a record and its newline separately under a per-instance lock, so two journals interleave onto one line and fail to parse on replay — bricking the *next* boot |
| At-most-once effect keys | Held in memory, populated at `load()`. A second instance never sees the first's commits, so a send or a spend runs twice |
| `serial` / `task_writes` | Per-instance mutexes. Two of either and both invariants lapse across the swap |
| `FsEventLog` | Derives `seq` from a line count under its own lock, and its broadcast sender is what an open console SSE stream is subscribed to |
| `FeedbackFiler` | In-memory rate limiter; rebuilding it makes a rebuild loop a rate-limit bypass |
| `HarnessPool` | Holds each agent's conversation history |
| `McpRuntime` | Dials a *process-global* connection map keyed by server id; re-booting it replaces connections other agents may be mid-call on |
| `ManifestApprovalGate` + `GrantSet` | Parked approvals and unredeemed single-use grants. Rehydrating a fresh copy from the journal resurrects what the live one has already resolved |
| `OpsStores` (incl. the `RunStore`) | Carried whole, so the dispatch choke point, the successor's `HarnessBrain` and the (suppressed) reaper all address one set of attempt rows. A second store would strand every live run and restart attempt ordinals at 1 |

Deliberately **not** carried over: the brain, tools, channels, workflow runner
and economy (replacing those is the point), and the in-flight steer registry
(the successor's harness deps mint their own; sound only because the swap
happens after the drain, so the outgoing registry is empty).

## Boot-only side effects, suppressed on a rebuild

`build()` treats the presence of a handover as "this is a rebuild" and skips:

- **Journal replay** (`load()`) and **approval rehydration** — the inherited
  journal and gate are already live.
- **Orphan-run reaping.** `reap_orphaned_runs` rests its whole argument on
  "nothing from this process can be in flight yet" — true at boot, false the
  moment a company has been serving.

  The exposure is narrower than it first looks, and worth stating exactly. The
  quiesce drains the serial lock, and both `begin_run` and the terminality
  backstop sit *inside* it, so no `Running` row survives the drain. `Pending`
  does: the dispatch choke point mints its row **outside** that lock, so a board
  write landing in the window leaves one behind. Reaping that row stamps the
  wrong reason on it, and if the rebuild then fails and `resume()` puts the
  company back to work, the row is already terminal — its cycle's `begin_run` is
  rejected and a genuinely live attempt runs with no record at all.

  Suppressed, therefore, rather than justified by the drain: relying on the
  drain would rest correctness on the current call order instead of on the
  invariant the reaper actually states. The port's doc comment now says the
  proof is about boot and only boot, so a future second caller has to make the
  argument again rather than inherit it.
- **Workspace seeding** — the company is already running.
- **Going public.** A paid, networked handle claim. A company that is already
  public does not become more public by claiming again.
- **MCP boot** — see the table above.

## What happens to in-flight work

The cycle in flight at the quiesce **completes** on the outgoing runtime,
against the same journal, approval queue and stores the successor adopts. It is
not cancelled, and its effects are not replayed: the executed-key set travels
with the journal.

Cycles arriving *during* the window are refused with `503 quiescing` rather than
queued, so a caller retries against the successor instead of silently getting
the brain the rebuild was replacing. The window is one turn wide.

That is also the bound on the triggering request: `PUT …/inference` blocks until
the in-flight turn drains, so a save landing mid-turn waits for it. This is a
deliberate trade. Swapping while a cycle runs is precisely the two-live-runtimes
hazard the handover exists to avoid, and a bounded wait that gave up and swapped
anyway would trade a slow response for a corrupted journal. In practice the
transition this feature exists for — first-time BYOK setup on a company running
the echo brain — has no expensive turn to wait on.

Parked approvals and unredeemed grants survive untouched, with their ids, parked
effects and TTLs. Resolving an approval after the swap runs its follow-up cycle
on the *new* brain — which is the desired outcome, not a compromise.

### Attempt rows (#242)

The same answer, extended to the run write path, because a run is written at
three points a swap can fall between.

The **attempt in flight** is the cycle in flight, so it completes and settles
itself on the outgoing runtime, against the store the successor adopts. Its
trace keeps writing through the drain: the sink holds an `Arc<dyn RunStore>` from
the inherited `OpsStores`, not a handle to the runtime.

The case that needed a code change is the **attempt that never starts**. Board
writes are deliberately *not* gated on the quiesce — only cycles are — so a card
dragged into `in_progress` during the window still passes the dispatch choke
point and still mints its `Pending` row. Its cycle is then refused, and the
refusal happens in `CompanyRuntime::run_cycle` *before* `CycleRunner` takes the
serial lock and therefore before `begin_run` — which puts it out of reach of the
terminality backstop, since that only settles rows the cycle itself started.
Left alone the row would claim to be pending for the rest of the process's life,
and the rebuild has just switched off the one sweep that would have cleaned it
up.

So the dispatch settles it: `Pending` → `Failed` carrying
`ports::runs::RUNTIME_REPLACED_ERROR`, a reason string kept distinct from
`ORPHAN_ERROR` so a run list can tell "we swapped your runtime, try again" apart
from "the host died". `Pending` → terminal is already the legal move the status
table names for *a dispatch that failed before the first turn*. The card stays in
`in_progress`, which is where every other failed dispatch cycle leaves it.

A rebuild that fails leaves the outgoing runtime registered and **resumes** it. A
company stuck quiesced would refuse every cycle forever, which is strictly worse
than the stale brain the rebuild was trying to replace.

## Registry-driven pollers

A registry swap only helps surfaces that read the registry. `WorkflowScheduler`
already re-read it every tick; `CompanyScheduler`, `MailboxPoller` and
`TelegramPoller` each snapshotted an `Arc<CompanyRuntime>` at boot and would have
kept driving the replaced — and now quiesced — runtime forever. Each gained a
`following(registry)` opt-in that the boot path uses; the snapshot remains the
fallback, so existing embedders are unaffected.

Cron is the one that matters most here: "scheduled workflows never fire" is one
of the two surfaces #266 was reported against, so a rebuild that did not reach
the scheduler would look fixed and not be.

## Wiring

`RuntimeRebuilder` is a trait implemented by the **binary**, because the builder
inputs (harness pool, OpenHuman RPC transport, managed media/search backends,
the manager-injected per-tenant mailbox) are assembled there from the process
environment and feature flags. `AppState::with_rebuilder` installs it, and
`AppState::set_boot_inputs` stashes what a rebuild cannot recover any other way
(`--discoverable`, the source directory).

A host that wires **no** rebuilder keeps the pre-#290 behaviour exactly: the
status still reports `restartRequired`, and the console still says so. That is
the honest answer when a rebuild genuinely is unavailable, so the flag and the
banner are retained rather than deleted.

## Related

- #266 — the honest-message half, and where the console reports `restartRequired`
- [manifest.md](manifest.md#inference) — `[inference]` precedence and the
  next-turn guarantee
- [lifecycle.md](lifecycle.md) — company lifecycle states (`paused`, `archived`),
  which are durable and unrelated to the transient quiesce window
