# MCP tiered tool-call permissions — design + rollout plan

Tracking issue: #2373.

This is an **implementation brief**, not a permanent architecture doc — it lives
under `docs/issues/` rather than `docs/modules/` on purpose. `docs/modules/mcp.md`
describes the system as it exists; this describes a piece of work not yet done.
Once the work lands, the relevant parts of this brief should be folded into
`docs/modules/mcp.md` and this directory can be deleted.

**Who this is for:** an engineer or a fresh Claude Code session with no prior
context on this effort. Read the files in order. `01-prerequisites.md` alone
should be enough to execute the first slice of work correctly without reading
anything else in this repository first — start there if you only read one file.

## The problem, briefly

Today, MCP tool-call approval is a single flat list:
[`read_only_tools`](../../modules/mcp.md) on a server declaration. A tool named
in that list gets called without a human in the loop (downgraded to
`Reach::ExternalRead`); every other tool on that server parks for approval
(`Reach::Consequence`). One tier, one list, set once per server.

Claude's own Connectors settings page (`claude.ai` → Settings → Connectors →
a connector's detail page) does this better: every tool a connected server
exposes is auto-grouped into three risk tiers — **Interactive**, **Read-only**,
**Write/delete** — each tier has a sensible bulk default (read tools default to
always-allowed, write/delete tools default to needing approval), and any
individual tool can still be overridden independently of its tier's bulk
setting. This brief designs the equivalent for OpenCompany: a real per-tool
tier + override model, backing a console page shaped like Claude's.

Getting there needs three things in order: closing a couple of loose ends in
our current MCP implementation first (§`01-prerequisites.md` — otherwise the
new permission data would be built on top of a reach model that doesn't
actually enforce what it claims to), designing the actual tier/override data
model (§`02-tool-tier-model.md`), and then the storage/API/rollout mechanics
(§`03-storage-api-rollout.md`).

## What this looks like when it ships

Illustrative only — not a component spec, and not pixel-accurate. These are
here so the implementation detail in the other three files has a concrete
target to point at.

**The servers list** — mostly what exists today (`McpServersSection.tsx`),
shown for orientation before the new pieces below it:

```
┌─────────────────────────────────────────────────────────────────────┐
│  MCP Servers                                    [Search]   [+ Add]  │
├─────────────────────────────────────────────────────────────────────┤
│  Your servers                    Directory                          │
├─────────────────────────────────────────────────────────────────────┤
│  Server            Source        Status         Reach               │
│  ─────────────────────────────────────────────────────────────────  │
│  notion             manifest      ✓ connected    2 agents      ›    │
│  github-issues      console       ✓ connected    all agents    ›    │
│  slack-internal      console      ⚠ needs auth   1 agent       ›    │
│  linear (directory)  registry     ✓ connected    3 agents      ›    │
└─────────────────────────────────────────────────────────────────────┘
```

**Adding a declared server** — the existing "paste a URL" flow
(`03-storage-api-rollout.md` §API), largely unchanged by this brief:

```
┌───────────────────────────────────────────┐
│  Add MCP server                        ✕  │
├───────────────────────────────────────────┤
│  Name                                      │
│  ┌─────────────────────────────────────┐  │
│  └─────────────────────────────────────┘  │
│                                             │
│  MCP server URL                            │
│  ┌─────────────────────────────────────┐  │
│  │ https://mcp.example.com/mcp          │  │
│  └─────────────────────────────────────┘  │
│                                             │
│  ⚠ Only connect servers you trust.         │
│    OpenCompany cannot verify what tools    │
│    a server exposes or whether they        │
│    change after you connect.               │
│                                             │
│              [ Cancel ]     [ Continue ]   │
└───────────────────────────────────────────┘
```

**Browsing the directory** — installing a pre-built connector from the public
registry instead of typing a URL (`03-storage-api-rollout.md`'s
registry-installed path). The ✓ badge is the *directory's* verification
signal, independent of whether this company has installed it — that's what
`[ + ]` vs `[ ✓ Installed]` tracks. The unbadged, explicitly-labeled Postgres
card is deliberate: this is browsing the public, unvetted
`modelcontextprotocol/registry`, and `02-tool-tier-model.md` already argues
that self-reported server metadata can't be trusted for enforcement — an
unverified entry should look visibly different from a verified one, not just
lack a checkmark. Installing here is what leads into the tool-permissions
page below.

```
┌──────────────────────────────────────────────────────────────────────┐
│  ← MCP Servers        Discover                          [+ Add]      │
├──────────────────────────────────────────────────────────────────────┤
│  Your servers    │  Discover                                         │
├──────────────────────────────────────────────────────────────────────┤
│  [Search the registry...........................]   [Filter: All ▾] │
│                                                                        │
│  Top connectors                                        [Show all]    │
│  ┌────────────────────────────┐  ┌────────────────────────────┐     │
│  │ 🐙 GitHub               ✓ │  │ 📋 Linear                ✓ │     │
│  │ Search, read, and open     │  │ Read and update issues,    │     │
│  │ PRs and issues               │  │ manage cycles               │     │
│  │                     [ + ] │  │                     [ + ] │     │
│  └────────────────────────────┘  └────────────────────────────┘     │
│  ┌────────────────────────────┐  ┌────────────────────────────┐     │
│  │ 💬 Slack                 ✓ │  │ 📝 Notion                ✓ │     │
│  │ Send messages, search      │  │ Search, read, update pages │     │
│  │ channels                    │  │ across your workspace       │     │
│  │                     [ ✓ Installed]│                [ + ] │     │
│  └────────────────────────────┘  └────────────────────────────┘     │
│  ┌────────────────────────────┐  ┌────────────────────────────┐     │
│  │ 🗄  Postgres              │  │ ☁  Custom (no directory     │     │
│  │ Unverified — community     │  │    listing found)          │     │
│  │ published                   │  │  Paste a server URL         │     │
│  │                     [ + ] │  │  directly instead → [+ Add] │     │
│  └────────────────────────────┘  └────────────────────────────┘     │
└──────────────────────────────────────────────────────────────────────┘
```

**The tool-permissions page** — the actual centerpiece this whole brief
exists to enable, per-tool tiers from `02-tool-tier-model.md`. "↺ reset" only
appears on a row the operator has explicitly overridden (the DTO's
`isOverride` field from `03-storage-api-rollout.md`); everything else is
inheriting its tier's bulk default. "Block" is a genuinely new enforcement
state — today the system can only let a call through or make an agent ask a
human first; it has no "refuse outright."

```
┌──────────────────────────────────────────────────────────────────────┐
│  ← MCP Servers        notion            [manifest]     [Disconnect]  │
├──────────────────────────────────────────────────────────────────────┤
│  Reachable by: research-agent, support-triage        [Edit agents]   │
│                                                                        │
│  Tool permissions                                                     │
│  Suggested tiers below are a starting guess, not enforced — set      │
│  your own to override. "Blocked" refuses the call outright.          │
│                                                                        │
│  ▾ Write / delete tools  (3)              [Needs approval ▾] (tier)  │
│  ──────────────────────────────────────────────────────────────────  │
│     delete_page              suggested: write/delete                 │
│                               [ Allow ] [●Approval] [ Block ]         │
│     archive_database          suggested: write/delete   ↺ reset       │
│                               [ Allow ] [ Approval] [●Block]          │
│     update_page               suggested: write/delete                │
│                               [ Allow ] [●Approval] [ Block ]         │
│                                                                        │
│  ▾ Interactive tools  (2)                 [Needs approval ▾] (tier)  │
│  ──────────────────────────────────────────────────────────────────  │
│     create_comment            suggested: interactive                 │
│                               [ Allow ] [●Approval] [ Block ]         │
│     move_page                 suggested: interactive     ↺ reset      │
│                               [●Allow ] [ Approval] [ Block ]         │
│                                                                        │
│  ▾ Read-only tools  (11)                  [Always allow ▾] (tier)    │
│  ──────────────────────────────────────────────────────────────────  │
│     search_pages              suggested: read-only                   │
│                               [●Allow ] [ Approval] [ Block ]         │
│     get_page                  suggested: read-only                   │
│                               [●Allow ] [ Approval] [ Block ]         │
│     list_databases            suggested: read-only                   │
│                               [●Allow ] [ Approval] [ Block ]         │
│     ⋯ 8 more                                                          │
└──────────────────────────────────────────────────────────────────────┘
```

## Contents

| File | Covers |
|---|---|
| [`01-prerequisites.md`](01-prerequisites.md) | Four fixes to land first (or note as independent/deferred), with exact functions/files and why the order matters |
| [`02-tool-tier-model.md`](02-tool-tier-model.md) | Where a tool's tier comes from, the new Rust types, the `mcp_call_tool_consequence` rewrite, and the migration rule for existing `read_only_tools` data |
| [`03-storage-api-rollout.md`](03-storage-api-rollout.md) | Why declared-server and registry-install storage stay split, the new API routes, a one-paragraph frontend note, and the six-step rollout order |

## What this brief does not cover

- Any of the actual code changes — this is documentation only. No PR from this
  branch touches `crates/` or `frontend/src/`.
- Reading MCP's own `readOnlyHint`/`destructiveHint` tool annotations directly
  from a remote server — real, but out of scope; see the last section of
  `02-tool-tier-model.md` for why and when to pick it up.
- A UI mockup or component-level frontend spec — `03-storage-api-rollout.md`
  says what the backend needs to expose and confirms the current console has
  no equivalent view today, but the actual screen design is separate work.
