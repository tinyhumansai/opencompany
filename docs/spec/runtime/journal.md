# The runtime journal and its storage port

The runtime journal (`src/runtime/journal.rs`) holds five things no other store
holds:

- the **at-most-once effect set** — the idempotency key of every side effect that
  was committed to run;
- the **parked-approval queue**, with each approval's original `ApprovalId`, the
  effect it parked, its board task and its conversation;
- **single-use and standing grants**, minted when an operator approves a tool
  call;
- **cycle brackets**, which is how a cycle interrupted by a host restart is
  recognised and settled at the next boot;
- **blocked agent-node continuation state** (issue #1816/#1825) — the workflow
  id and trigger input a blocked node's gated tool call needs to re-dispatch
  its run after a restart (`BlockedNodeStashed`), plus the markers
  (`BlockedNodeApproved`, `BlockedNodeDispatched`, `BlockedNodeReleased`) that
  let `reconcile_stranded_blocked_nodes` tell a still-pending stash apart from
  one already resumed at boot.

It is deliberately not the [`EventLog`](events.md). `CompanyEvent` is a closed,
binding enum with no marker variants, and the event log is *pruned* under a
retention policy — rotating away an `EffectExecuted` key would silently
un-commit it and let an at-most-once effect fire a second time.

## Why it is a port (issue #726)

The journal used to be constructed unconditionally on the filesystem, at
`<home>/companies/<slug>/journal.jsonl`, outside the port surface
`StorageHandles` swaps for every other durable store.

On a hosted tenant with `OPENCOMPANY_STORAGE=mongodb` the container's `/data` is
documented **ephemeral scratch** — the same fact the TinyCortex boot refusal
exists for (see
[memory-engine.md](memory-engine.md#durability-contract--the-data-is-scratch-caveat)).
Container replacement — a deploy, a reschedule, a node drain, an OOM kill —
discarded the file. Every previously executed effect became eligible to fire
again, and every parked approval, grant and standing grant silently vanished.

That is a data-integrity defect, not log noise, so the fix is the port
(`src/ports/journal.rs`) rather than a warning.

## `JournalStore` — two byte-level operations

```rust
async fn append_journal(&self, id: &CompanyId, line: &str) -> Result<()>;
async fn read_journal(&self, id: &CompanyId) -> Result<Vec<String>>;
async fn journal_imported(&self, id: &CompanyId) -> Result<bool>;
async fn complete_import(&self, id: &CompanyId, lines: Vec<String>) -> Result<()>;
```

Not a mirror of `RuntimeJournal`'s ~25 methods. The journal's whole persistence
contract is "append one opaque line, read every line back in order". Everything
semantic — the record enum with its `#[serde(default)]` archaeology, the
corrupt-line skip, the merged-line recovery, the replayed in-memory state — stays
in `RuntimeJournal` and is backend-agnostic by construction. A backend stores
strings and never learns what a `JournalRecord` is, so a new record variant needs
no backend change.

Two obligations a backend must not weaken:

- **Durability before return.** The at-most-once guarantee is that a key is
  durable *before* the side effect runs. An `Err` therefore reaches the caller
  before the effect, which fails closed. The residual case — a timeout on a write
  the server did commit — leaves a committed key with no effect, the contract's
  documented safe direction.
- **Bytes and order.** A rewritten, trimmed or re-encoded line no longer parses,
  and a skipped `EffectExecuted` un-commits its key. A park read back *after* the
  resolution that drained it resurrects a resolved approval.

`src/store/conformance.rs` holds every backend to both:
`assert_journal_store` (all three backends) and `assert_journal_import`
(sqlite and mongodb — the fs backend has nothing to import). Both levels of
`Durability` are exercised there, because a backend that ignored the parameter
would otherwise pass every assertion.

## Durability travels through the port (issue #392)

Durability is chosen **per record kind**, not once for the journal:
`EffectExecuted`, `GrantConsumed` and `StandingGrantRevoked` must survive losing
the machine, because losing them makes the runtime repeat an external action —
as must the blocked agent-node continuation records (`BlockedNodeStashed`,
`BlockedNodeReleased`, `BlockedNodeApproved`, `BlockedNodeDispatched`, issue
#1816/#1825), because losing one strands a decided approval with no re-park
coming. Every other kind need only survive the process, because losing it makes
the runtime *re-ask*. The full per-record decision and its reasoning live in
[lifecycle.md](lifecycle.md#which-crash-a-journal-record-survives-issue-392).

`append_journal` therefore carries a `Durability`, and a backend MUST NOT flatten
the two levels into one. Flattening upward taxes the journal's highest-volume
records with a flush they do not need; flattening downward silently drops the
guarantee that keeps an already-fired effect from firing again after a power
loss, which is the one thing the split was bought for.

The fs backend's directory handling is part of that contract rather than an
implementation detail: a `Host` append into a fresh data directory creates the
parent chain through `create_dir_all_durable`, which flushes each created
directory's own parent. A record flushed under directory entries that were never
written down is lost with them — on exactly the crash the flush was bought for,
with the record's own `sync_data` still passing its own test. That is why the
`RuntimeJournal` → `JournalStore` move kept both halves of the branch together.

## Backend shapes

| Backend | Shape | Ordering | Durability |
|---|---|---|---|
| `fs` | `<home>/companies/<slug>/journal.jsonl` | file order; one whole-line `O_APPEND` write per record, serialised on a process-wide per-path lock | `Process`: the write syscall has completed. `Host`: plus `sync_data`, plus a flushed parent chain |
| `sqlite` | `journal(company_id, seq, line, PK(company_id, seq))` + `journal_imports` | `seq` from `COALESCE(MAX(seq)+1, 0)` in the insert statement | `Process`: WAL commit under `synchronous=NORMAL`. `Host`: the commit runs under `synchronous=FULL` |
| `mongodb` | `journal` collection `{company_id, seq, line}`, unique `(company_id, seq)` + `journal_imports` | `seq` from the atomic `counters` allocator `EventLog` already uses | `Process`: an acknowledged `insert_one`. `Host`: the same with `j:true` write concern |

The mongodb collection is **not capped**. Capping drops the oldest documents, and
the oldest documents are the at-most-once keys of effects that ran months ago;
un-committing one re-arms that effect. Bounding the journal is a retention
question with a correctness argument attached, not a storage-shape default.

## Migration: one-time, receipt-gated, verbatim

The fs implementation *is* today's file at today's path, so the default backend
migrates nothing.

For sqlite and mongodb, `RuntimeBuilder` runs a one-time import at construction:

1. ask the sink whether this company has an import receipt;
2. if not, read `journal.jsonl` — **raw strings, in file order**, so a corrupt or
   merged line migrates byte-for-byte and the journal's own recovery still
   applies to it;
3. `complete_import` **clears, copies, then writes the receipt**, in that order.

The order carries the safety argument. An import interrupted anywhere leaves the
gate open, so the next boot re-runs the whole clear-and-copy. What must never
happen is a *partial* copy behind a *closed* gate: a journal missing its tail is
a set of at-most-once keys that quietly went missing — the exact bug the port
exists to fix. On sqlite the three steps are one transaction; on mongodb they are
not, and the receipt landing last is what makes the non-atomic case safe.

A failed import is **fatal to boot**. Coming up with an empty journal is
indistinguishable, to every effect the company then runs, from having never
executed anything.

Two deliberate choices around the gate:

- **The source file stays in place.** The receipt makes a second import
  impossible, and a rollback to an older binary still finds the history it knows
  how to read. Renaming it would hand that binary an *empty* at-most-once set.
- **The gate closes even with nothing to import** (an import of zero lines), so a
  `journal.jsonl` that appears *later* — a rollback, a stray copy into the data
  dir — can never wipe and replace a journal the backend has since accumulated.

On a tenant whose container was already replaced there is nothing on disk to
import, and nothing can retroactively recover keys that were already lost.

## Concurrency

- **Within one `RuntimeJournal`**, appends serialise on a write lock held across
  the store call — which is what keeps a backend's sequence allocation in call
  order, and therefore keeps a park from replaying after its resolution.
- **Across instances in one process**, the fs backend's per-path lock still
  applies. A database backend allocates sequences server-side, so two live hosts
  interleave without collision: the same physics as `O_APPEND`.
- **Single writer per company remains the documented contract**
  ([data-root.md](data-root.md#single-writer-enforced)). One gap is *pre-existing*
  and out of scope here: `RuntimeJournal`'s `is_executed` set is per-process
  in-memory state, so two simultaneously live containers on one tenant database
  would each answer from their own replay. Moving the journal into a shared
  backend does not create that gap and does not close it.

## No interim boot refusal

The TinyCortex refusal ([memory-engine.md](memory-engine.md)) guards an *opt-in*
engine that has alternatives to point at. The journal is unconditional and has no knob, every
staging tenant runs mongodb, and a refusal would take the hosted fleet down to
protect it from a risk it is already living with — while stranding tenants with
no remedy. What is adopted from that precedent instead is **fail-loud
selection**: under a database backend the journal sink comes from the backend
handles, never a silent fs fallback, matching the rule `open_storage` already
states for every other port. The boot log names the sink it resolved.
