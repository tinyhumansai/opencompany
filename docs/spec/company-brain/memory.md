# Memory

What a company remembers, where it lives, and the Operator's rights over it.

## What is remembered

| Kind | Written by | Retention |
| --- | --- | --- |
| Compressed cycle traces | every cycle | newest 32 retained and inspectable; backends that archive rather than destroy bound the archive tier to the newest 32 evicted traces too; **not** summarized or read back into a cycle (issue #1175) |
| Task results (delegated work products) | cycles | durable |
| Context chunks (documents, research, transcripts the brain filed) | cycles, imports | durable, content-addressed |
| Customers, engagements, decisions, outcomes | cycles (as structured task results / context) | durable |
| Feedback items and their issue links | feedback flow | durable |

Conversation history with the hosted brain also exists server-side per
session ([integrations/medulla.md](../integrations/medulla.md)); the local
stores remain authoritative for export and migration.

## Port boundary

Memory spans three ports, not a database
([runtime/ports.md](../runtime/ports.md)):

- **`MemoryStore`** — the brain's own traces and task results; the shape of
  Medulla's `CyclePersistence` (`save_trace`, `recent_traces`,
  `save_task_result`, `evict`).
- **`ContextStore`** — the RLM environment (`put`/`list`/`peek`/`search`)
  the brain queries lazily instead of stuffing context windows.
- **`FactStore`** — the **operator's** durable, hand-curated Memory view: the
  facts, preferences, people, projects, and references the console's Memory
  surface lists, searches, adds, and deletes (`list`/`upsert`/`delete`). This
  is distinct from the two cognition ports above — it is a person-authored
  record, not compressed cognition — and is not fed into the cycle loop the way
  traces are.

The first two ports are the brain's memory; `FactStore` is the operator's. All
three key on `CompanyId` and travel with the export bundle.

Every `ContextStore` chunk carries `ChunkMeta::stored_at_millis`, the wall-clock
time it was stored; a chunk written before backends recorded one reports `0`.
The console's Brain header needs it: agents write memory **only** through the
`ContextStore`, so a freshness figure drawn from `FactStore` alone reads as
"never updated" for any company whose operator has not hand-authored a fact.
`GET /memory/stats` therefore reports `lastUpdatedAtMillis` as the max across
both ports, alongside the facts-only `factsUpdatedAtMillis`.

The stats response exposes the Brain's full-store display partition: `facts`,
`teammateMemory`, `taskOutcomes`, and `documentMemory` are disjoint, and
`totalItems` is their sum. Operator-fact context mirrors remain
agent-recallable but are not counted as teammate memory, because the
corresponding fact is already the operator-visible row. Dropped-document and
link chunks (`document/…`) get their own bucket for the same reason: they are
operator-supplied material, and counting them as teammate memory would read
as something an agent learned.

`GET /memory` returns its rows as `items` alongside the truncation metadata
for that same read: `totalContext` — the non-mirror context chunk population
before the 500-row list cap — and `contextTruncated`, whether the rows dropped
any to it. Facts are never capped and are not counted in `totalContext`.
Keeping the two beside the rows is what makes the console's "showing the
newest N of M" notice consistent: N (the rows rendered) and M (the uncapped
count) come from one server-side read, so a write between requests can never
make them disagree. A `?query=` request is the exception: its rows are search
matches, not "the newest N", so the metadata is reported as not applicable
(`totalContext: 0`, `contextTruncated: false`) rather than implying a search
result was dropped by the cap. The stats counts above are never capped; the
list may show only the newest 500 non-mirror context rows.

Read that stamp as a max across chunks, not as one row per body: one address
carries one row per *label* claiming it (issue #1300), a new label on an
existing body stamps per-label on fs/sqlite and keeps the address's first
stamp on the single-record backends (mongodb, the provider facade), and
neither the export bundle nor a restore preserves it — a restored chunk is
stamped when it lands.

**A hosted provider is an optional backend for `MemoryStore` and
`ContextStore`** ([memory-engine.md](../runtime/memory-engine.md)) but is a
choice, not a dependency: the fs default preserves the one-key promise, and
DB-agnosticism applies to memory exactly as to every other store.

## Compounding

**Intended**: compressed traces will eventually provide a compact, durable
working-memory input that biases future work — the mechanism behind "memory
compounds" in the [vision](../vision/README.md). That requires a real
summariser and a consumer; neither exists today.

**Today** (issue #1175), one narrower path does the compounding:

- Before each turn the harness retrieves the top-5 prior task outcomes matching
  the incoming message from the `ContextStore` and injects them as text, then
  stores the turn's outcome back (`src/harness/memory_loop.rs`). This is the
  only live recall a company has.
- Traces are written every cycle and read by nothing. `CycleRequest` used to
  carry them; no `Brain` consumed the field, so it was removed rather than left
  looking functional.
- The process-wide maintenance pass retains the newest 32 traces with
  `MemoryStore::evict(KeepRecent)`. The policy bounds trace storage — including
  on backends whose `evict` archives rather than destroys, where the archive
  tier is capped by the same policy — while the summaries are still
  placeholders; it is not a claim that traces compound.
  `ContextStore::delete` reaps an operator fact's mirror and forgets a dropped
  document, but nothing sweeps the chunk store on age, so it grows until
  something deletes by name.

## Dropping documents and links in (the Brain drop zone)

An operator can put a file, a whole folder, or a link into memory by dropping
it on the Brain page. The host extracts its text, chunks it, and writes the
chunks to the `ContextStore` under a `document/{slug}/{index}` label — the
same store agent recall reads, so a dropped handbook reaches a teammate on its
next turn with no further step (`src/ingest`,
`src/server/ops/memory_ingest.rs`).

Four properties are normative, because each is a way this could quietly lie:

- **The text is stored, the file is not.** Memory keeps what the document
  *said*. Files an operator wants back live in the workspace tree; a second
  copy of every upload there would make the Brain page a silently diverging
  file manager. Re-dropping is how a document is corrected.
- **Extraction never guesses.** A format the build cannot read is reported as
  unsupported per file, never stored as decoded noise that would count as a
  memory and recall as nothing. Text, Markdown, HTML, JSON and CSV need no
  parser; PDF, `.docx`, `.xlsx` and `.pptx` ride the `documents` feature (in
  the default set). A scanned PDF with no text layer is *empty*, which is a
  different answer from *failed* and is said differently.
- **Every file gets its own row in the answer.** A real folder always contains
  a `.DS_Store`, an image, or a scanned page; failing the batch over one would
  make folder drops unusable, and skipping it silently would leave the
  operator believing the whole folder is in memory.
- **A drop can be taken back.** `DELETE …/memory/document/{slug}` forgets every
  chunk of one document — the Delete right below, applied to the one context
  origin an operator authored. The delete is label-scoped (#1300): a chunk
  whose byte-identical body is also indexed under another label loses only
  this document's claim on it, so somebody else's row survives and the body
  is reaped with its last claim. It previously *skipped* such a chunk, which
  left a forgotten document's own rows listed for good.

Links are fetched **by the host** (a browser cannot, cross-origin), so the URL
path is guarded server-side: `http`/`https` only, and never a loopback,
link-local or private address — "remember this page" must not become a read
primitive against the deployment's own network.

## Operator rights (normative)

- **Inspect**: `GET /api/v1/companies/{id}/memory/traces` exposes the retained
  cycle-trace window, and `GET …/memory/archives` exposes the traces a
  provider-backed engine retained when eviction dropped them from that window
  (a store/embedded engine keeps no archive and answers `404`). The exported
  bundle exposes everything the selected backend retains, human-readably.
- **Delete**: the Operator MAY delete any memory item, context chunk, or
  `FactStore` fact; deletion propagates to the backing store and is journaled
  to the `EventLog` (that a deletion happened is auditable; the content is
  gone).
- **Redact**: customer-content redaction requests are honored across traces
  and chunks — required for the privacy stance in
  [feedback-loop/privacy.md](../feedback-loop/privacy.md).
- **Export**: memory travels with the bundle; no store may hold memory
  hostage ([runtime/lifecycle.md](../runtime/lifecycle.md), export).
