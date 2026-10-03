# Integrations

How OpenCompany composes its neighbor systems, and the rules that keep it a
thin kernel instead of a fork farm.

## The reuse-first rule (normative)

OpenCompany MUST NOT reimplement what a neighbor already provides. When a
neighbor lacks something the runtime needs, the fix is a PR against that
neighbor's repo — never a local fork or a parallel implementation. Candidate
upstream workstreams are tracked in [roadmap.md](../roadmap.md) and in each
integration doc.

The corollary: every neighbor sits behind a kernel port
([runtime/ports.md](../runtime/ports.md)), so being *preferred* never means
being *required*.

## Dependency matrix

| System | Doc | Class | Without it |
| --- | --- | --- | --- |
| Medulla via TinyHumans backend | [medulla.md](medulla.md) | **required for cycles** | build/inspect/explore only |
| OpenHuman | [openhuman.md](openhuman.md) | default tools/channels | built-in tools; extra channels disabled |
| TinyAgents | [tinyagents.md](tinyagents.md) | default harness (feature `tiny`) | stub brain and local workers unavailable |
| Hosted memory | [memory-engine.md](../runtime/memory-engine.md) | optional memory backend (feature `tinymemory`) | fs memory bundle |

## Vendoring and versioning

- `vendor/openhuman` is a git submodule; OpenHuman nests TinyAgents and
  TinyCortex as its own submodules.
- Published crates are preferred where they exist: `tinyagents = "2.1"`
  (path-patched to OpenHuman's nested submodule via `[patch.crates-io]`).
- OpenHuman is embedded behind the `openhuman` feature for the agent harness;
  the desktop launcher remains available through `opencompany open-human`.
- Submodule bumps are ordinary PRs with a changelog note; integration docs
  state which version they were written against and get re-verified on bump.
