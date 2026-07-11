# TinyCortex

TinyCortex is the TinyHumans memory layer — the intended durable backend for
the company brain's memory. It is registered as an OpenHuman submodule
(`vendor/openhuman/vendor/tinycortex`) but is not checked out in this repo's
default state, so this document specifies **expectations against the kernel
ports**, not TinyCortex internals. When the adapter is written (feature
`tinycortex`, Phase 6), this doc gets re-verified against the real API.

## Role

Implement the two memory ports
([runtime/ports.md](../runtime/ports.md)) for companies that outgrow the fs
bundle:

- **`MemoryStore`** — compressed cycle traces and task results. Requirements
  the backend must satisfy: append traces keyed by company + cycle, return
  the N most recent efficiently (this is on the hot path of every cycle),
  count-or-age-based eviction that archives rather than destroys, and honor
  hard deletes ([company-brain/memory.md](../company-brain/memory.md),
  operator rights).
- **`ContextStore`** — the RLM environment: content-addressed `put`,
  prefix `list`, ranged `peek`, and relevance `search` over chunks. Search
  quality is the value proposition — embeddings/semantic recall is exactly
  what a real memory layer adds over the fs default's lexical search.

## Contract requirements (normative for the adapter)

- **Company isolation**: every operation is scoped by `CompanyId`; the
  adapter MUST NOT allow cross-company reads regardless of how TinyCortex
  namespaces internally.
- **Export completeness**: everything written MUST be readable back out so
  bundle export stays total
  ([runtime/lifecycle.md](../runtime/lifecycle.md)).
- **Deletion and redaction propagate** to the backing store, not just an
  index.
- **No new required credentials**: if TinyCortex runs as part of an
  OpenHuman deployment, it is reached through that seam; the one-key promise
  is unaffected.

## Fallback

The fs implementation (JSONL traces + content-addressed chunks with a
lexical index) is always compiled and remains the default. TinyCortex is a
quality upgrade, not a dependency.
