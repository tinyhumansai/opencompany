# 03 — Prerequisites (no code, or nearly)

Do these first. They change no runtime behaviour, they make every later slice
reviewable, and the first two close gaps that exist today whether or not any
feature is built. Each is small enough for one PR.

## 3.1 An owning doc: `docs/modules/skills.md`

**Problem.** `docs/modules/mcp.md` exists; skills, a subsystem of comparable
size, has nothing. It is described in passing across `globals.md:40`,
`ports-console.md:227-241`, `workspace-layout.md:135`, `harnesses.md`,
`api-write-plane.md`, `api-graphql.md`, `desktop.md` and others. Those fragments
agree with each other (no stale claim was found, unlike MCP's restart note), so
the fix is consolidation, not correction.

**Do.** Write `docs/modules/skills.md` from [`01-current-state.md`](01-current-state.md):
the data shape, the two-half storage, the effective-set fold, the route table,
materialization, and the read-only stance. Link it from `docs/spec/README.md`'s
index and from `docs/modules/`'s siblings. Replace each scattered paragraph with
a one-line pointer where it duplicates the new doc.

**Acceptance.** `docs/modules/skills.md` exists, ≤500 lines, every `file:line`
re-checked; the spec index links it.

## 3.2 A threat-model section for skills

**Problem.** `grep -ci skill` returns `0` for
`docs/spec/security/agent-isolation.md`, `docs/spec/company-brain/grants.md`,
`docs/spec/company-brain/approvals.md` and `docs/spec/runtime/tools.md`. But
`describe_skill` and `read_skill_resource` pull arbitrary text into an agent's
context, and for a registry-sourced skill that text was authored by someone other
than the operator. `agent-isolation.md` already treats a fetched page or an MCP
tool description that way ("the agent that reads them is the one holding the
grants"); skills are the same class and were simply never named.

**Do.** Add a short section to `agent-isolation.md`:

- registry- and upload-sourced `SKILL.md` content — body, description, and every
  bundled resource — is **untrusted text reaching an agent's context**;
- the exploitation path bottlenecks at a tool call, which is already gated by the
  three-level grant and `ApprovalGate`, so installing a skill is not itself a
  privilege grant — say so explicitly, because it is the reason skills are lower
  risk than MCP tools *today*;
- that stops being true the day execution ships ([`07`](07-execution-deferred.md)),
  and the section must say what has to change then.

Borrow structure, not text, from two published sources
([`02`](02-industry-comparison.md)): Anthropic's enterprise risk-tier table
(scripts, instruction manipulation, MCP references, network patterns, hardcoded
credentials, filesystem scope) and the Cloud Security Alliance's recommendations
(content-hash verification, approved registries, Unicode-tag stripping, signing,
least-privilege egress, audit log of skill changes). Add one line each to
`grants.md`, `approvals.md` and `tools.md` pointing at the new section and stating
that a skill neither adds a grant nor bypasses one.

**Acceptance.** All four files mention skills accurately; `agent-isolation.md`'s
"what must never be claimed" list does not imply skills are sandboxed.

## 3.3 Disambiguate the two meanings of "skill"

**Problem.** `[place].skills = [{ id, price_usd, description }]`
(`docs/spec/runtime/manifest.md:172`) declares priced tiny.place A2A capabilities,
served at `GET /a2a/{handle}/skill.md` (`docs/spec/runtime/api.md:388`). The
`SKILL.md` bundle is unrelated. Anyone grepping "skill" hits both, interleaved.
MCP had the same shape of problem (`mcp_call_tool` vs `mcp_registry_tool_call`).

**Do (docs only).** Add a one-paragraph signpost at the top of
`docs/modules/skills.md` and next to the `[place].skills` entry in `manifest.md`
and `api.md`: "a *priced capability* is an economy concept; a *skill bundle* is an
instruction document". Add both terms to `docs/spec/glossary.md`.

**Do not** rename the manifest key: it is a public route and manifest field
(`breaking` label territory). If a rename is ever wanted, that is its own issue.

**Acceptance.** Glossary has two entries; each surface links to the other.

## 3.4 Spec-conformant validation

**Problem.** The Agent Skills spec ([`02`](02-industry-comparison.md)) requires
`name` ≤64 characters, lowercase letters/digits/hyphens, matching the directory,
and `description` ≤1024 characters. OpenCompany has a hand-written parser
(`company/skill_file.rs:40`) and a slug check (`skill_effective::valid_slug`,
`company/skill_effective.rs:78`). Read on 2026-09-21: `valid_slug` requires a
leading lowercase letter or digit followed by lowercase letters, digits or
hyphens — but sets **no length cap** — and exists to stop path traversal, since a
slug becomes a directory name. `parse_skill_md` treats `name` as a display string
that only has to be non-empty, ignores unknown keys, and does not check that
`name` matches the slug. `MAX_SKILL_DOC_BYTES = 256 KiB`
(`server/ops/skills.rs:55`) bounds only the assembled document. So the spec's
64-character name limit, the name-equals-directory rule and the 1024-character
description limit are **not enforced**.

**Do.** Record the delta above in `docs/modules/skills.md`, then tighten in a
small code PR: cap slug length at 64, decide whether `name` must equal the slug
(the spec says yes; today `name` is display-only, so this is a compatibility
call for a human), cap `description` at 1024, and cap the frontmatter block. Keep the newline-collapsing behaviour that stops frontmatter
injection (`skills.rs:473-474`). Consider the optional Anthropic-only rules
(rejecting XML tags and the reserved words "anthropic"/"claude" in `name`) as
**optional**, not required by the standard.

**Ship the validator's rule set as data, not prose:** one function used by
`create_custom`, the upload path ([`06`](06-authoring-ux.md)) and the scan
([`05`](05-registry-trust-and-updates.md)), so the three cannot disagree.

**Acceptance.** Unit tests in `company/skill_file_tests.rs` cover: a 65-char name,
uppercase, a name that differs from the directory, a 1025-char description, a
description containing a newline plus a fake frontmatter key.

## 3.5 Order

3.1 and 3.2 first (they define what "correct" means for everything after);
3.3 folds into 3.1; 3.4 is the only item that touches code and can ship
independently. None of them blocks or is blocked by the per-agent work.
