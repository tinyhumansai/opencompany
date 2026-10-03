# One memory contract

*Phase P0. Replacing three bespoke ports and a hand-rolled client with
`tinymemory`.*

> **Status.** This page was written against TinyMemory v1 (`MemoryProvider`,
> capability families, a taint enum). The host has since migrated to
> TinyMemory v2, whose contract is `MemoryEngine`. The design intent below
> stands; the mechanics are updated to match the code. The operator-facing
> behaviour is in [../memory-engine.md](../memory-engine.md).

Terms: [glossary](../../glossary.md). Supersedes the backend half of
[company-brain/memory.md](../../company-brain/memory.md); the operator rights in
that document are unchanged and become requirements on the decorator specified
here.

---

## The situation

OpenCompany independently built a thin version of the seam `tinymemory` exists
to provide: a bespoke `CortexClient` trait with `CortexMemoryStore` and
`CortexContextStore` over it, alongside an in-process engine (removed in #1568).
`tinymemory` is already vendored — transitively, as a submodule of
`vendor/openhuman` — and OpenHuman binds it across hundreds of call sites, so
maintaining a second, weaker integration of the same thing bought nothing.

`tinymemory` v2 is an engine-neutral **contract** (`MemoryEngine`: `store`,
`fetch`, `list`, `forget`, `recall`, `health`) and a registry of engines built
with `build_engine`. The registry ships `cortexdb` (CortexDB direct) and
`tinyhumans` (CortexDB hosted by the TinyHumans backend). v2 dropped
namespaces and capability families: the only scoping an item has is its
metadata (`workspace`, `folder`, `source.id`, tags), filtered with `MetaFilter`.

## What changes

`MemoryStore`, `ContextStore` and `FactStore` stay as **typed facades** over one
engine rather than three independent storage backends. The ports stay because
their types are the company's vocabulary; the backends collapse because there is
no reason to have three.

| Port | Maps to | Notes |
| --- | --- | --- |
| `FactStore` | Document items in the facts scope | keyed by `source.id`; a rewrite stores the new item, then forgets the old |
| `ContextStore` | Document items in the context scope | `put` keyed by content address; `search` uses `fetch` (keyword preferred, else hybrid). Ranged `peek` is a host-side slice after a full read |
| `MemoryStore` | Document items in the traces scope | `evict` is a host-side move to an archive scope — see below |

The record bodies are a versioned JSON envelope; the facade owns the encoding
and every record type has a round-trip test (`src/store/memory/facades.rs`).

---

## The decorator is not optional

`tinymemory` deliberately owns no policy. Scope predicates, provenance, redaction
and audit belong to the host, on the path every caller takes — because an engine
that could be swapped for one that skips enforcement is the entire reason a
policy layer exists.

> **The three ports take `&CompanyId` as an explicit first argument. That is a
> compiler-enforced tenant-isolation invariant.** `MemoryEngine` has no tenant
> argument at all, only metadata. A missing workspace filter is a silent
> cross-tenant leak with no type-level guard.

So `store::memory::BoundMemory` is the **only** public way to get a company's
memory port, and it derives the namespace (`oc/<company>-<hash>/<scope>`) from
the `CompanyId` itself, per call. Every write stamps it as the item's
`workspace`, every read forces it into the filter, and every returned hit is
re-checked against it. Code that can name a raw namespace string does not exist
outside `store::memory`. Company A's facts MUST be invisible to company B.

The decorator additionally owns:

- **Per-agent and per-desk scoping.** Both cognition ports key on `CompanyId`
  alone, so there is no partition below the company; `BoundMemory` adds scoped
  context partitions for the [alignment layer](alignment.md).
- **The scratch firewall.** Provisional working-out lives in a sibling scope and
  is unreachable from durable recall **by construction**: each durable facade
  scopes its reads to its own namespace and drops anything outside it, rather
  than excluding scratch by name. Judging roles get neither half of it.
- **Archive on evict.** `evict` *archives rather than destroys*, and a test
  asserts it. The contract has no archive tier and no bulk delete by predicate,
  so eviction is a move between scopes, archive written before the live delete,
  and the archive is itself bounded. It MUST NOT be quietly downgraded to
  deletion.
- **Provenance.** Every inbound-channel write is marked external: source kind
  `Link` plus the tag `oc:provenance:external`. v1 carried a taint enum the
  engine stored; v2 has none, so the mark is metadata this host stamps and
  owns, and it survives `memory migrate` because items cross verbatim.
- **Operator rights.** Inspect, delete, redact and export from
  [company-brain/memory.md](../../company-brain/memory.md) are decorator
  responsibilities.

---

## Version pinning

Build against the **vendored** copy under
`vendor/openhuman/vendor/tinymemory/crates/tinymemory`, the copy OpenHuman itself
pins. Cargo unifies two path dependencies only when they resolve to the same
directory, which keeps one `MemoryEngine` trait identity in the process. The
in-pod `tinycortex` engine and its contract crate no longer exist at the pin.

OpenHuman's own memory domain is switched off on the shared runtime
(`host_domains` in `src/harness/openhuman_runtime.rs`): it is one unscoped engine
for the whole process, which would be a second, tenant-blind memory path beside
`BoundMemory`.

## What we gain beyond parity

- **One seam.** Three ad-hoc ports plus a bespoke client become one engine
  contract, with the engine chosen by configuration.
- **Ranked retrieval** from the engine, replacing a degraded lexical-and-recency
  path. [Demand dedup](demand-ledger.md#identity-and-dedup) depends on this — it
  is what turns a lexical hash into a semantic check.
- **Portability for free.** `opencompany memory migrate` copies items across
  engines with `list` then `store`, idempotent by content fingerprint.

## What we must not lose

- **Typed records.** The port types become encoded content on the way in; the
  facade owns the encoding and its round-trip test.
- **The isolation invariant.** It is the single largest risk in this phase.
- **Archive-not-destroy** and the ranged read. Both are host-side.

---

## Verification

- A cross-company namespace leak is unrepresentable: the decorator is the only
  constructor, and no call site can name a raw namespace.
- The existing eviction test still passes — `evict` archives, and does not
  destroy.
- Provenance survives an export and re-import.
- Scratch content is unreachable from durable recall, asserted against the
  recall path rather than against a routing table.
- Every facade round-trips its typed record through the engine without loss.
- The conformance suite still holds fs, sqlite and mongodb to identical answers
  for any port that keeps a non-engine backend.
