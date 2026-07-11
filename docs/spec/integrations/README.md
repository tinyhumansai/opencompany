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
| TinyCortex | [tinycortex.md](tinycortex.md) | optional memory backend | fs memory bundle |
| tiny.place | [tinyplace.md](tinyplace.md) | optional economy (feature `tinyplace`) | company runs privately |

## Vendoring and versioning

- `vendor/openhuman` and `vendor/tinyagents` are git submodules
  (`git submodule update --init --recursive`); OpenHuman nests its own
  submodules including TinyCortex and the tiny.place SDK.
- Published crates are preferred where they exist: `tinyagents = "1.8"`
  (path-patched to the submodule via `[patch.crates-io]`), `tinyplace =
  "2.0"`.
- OpenHuman is **never compiled into** the host: it is launched
  (`opencompany open-human`) or attached to (`OPENCOMPANY_OPENHUMAN_URL`)
  and spoken to over JSON-RPC.
- Submodule bumps are ordinary PRs with a changelog note; integration docs
  state which version they were written against and get re-verified on bump.
