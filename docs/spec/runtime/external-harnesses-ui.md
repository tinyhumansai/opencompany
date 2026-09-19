# External harnesses: the UI ↔ ACP-engine contract

How the console answers "what can I put a teammate on, and will it actually
run?" — which of the two halves comes from where, and why neither side stores
what the other knows.

Companion to [harnesses.md](harnesses.md), which covers the manifest side.

---

## Two half-answers, joined in the console

| | Answers | Scope | Route |
|---|---|---|---|
| **Host** | *which ids may an agent be bound to* | the company | `GET {scope}/harnesses` |
| **Machine** | *which coding CLIs are installed and signed in here* | this machine | Tauri `oc_acp_harnesses` → `acp::discovery::survey` |

Neither can answer the operator's actual question alone. The host cannot know
whether `claude-agent-acp` exists on the laptop asking; the desktop knows
nothing about the company. The console joins them on the harness **id**.

That the join works is deliberate, not luck: the manifest vocabulary
(`ACP_AGENTS`) and the discovery catalogue are the same three ids —
`claude`, `codex`.

```text
GET {scope}/harnesses  ──┐
   declared + detected   ├──▶  joinHarnesses()  ──▶  External harnesses page
oc_acp_harnesses ────────┘        (lib/harnesses.ts)
   readiness per id
```

---

## Declared versus detected

`HarnessDto.detected` splits the list in two, and the distinction is the whole
contract:

- **Declared** (`detected: false`) — a `[[harness]]` in `company.toml`. A
  property of the *company*: identical wherever the manifest is opened.
- **Detected** (`detected: true`) — a coding CLI this build can drive, bindable
  **without any declaration**. A property of the *machine*.

A local ACP harness is not something a version-controlled `company.toml`
should have to declare, because the same manifest is opened from machines
where the answer differs. So `Harness::implicit_local(id)` synthesizes the
harness a binding to a bare CLI name resolves to, and:

- it is **never `default`** — which harness an *unbound* teammate runs on
  stays a blueprint decision, so nothing a machine happens to have installed
  can silently redirect a whole roster;
- a **declared harness of the same id always wins**, so a company that pinned
  a model on its own `claude` harness is never shadowed by the bare one.

### There is no "connect"

A local CLI is usable exactly when it is installed and signed in. There is no
state in between for a button to move it through, and a stored "connected"
flag would be a second source of truth that could disagree with the CLI
actually being there. The page reports; it does not enrol.

There *is* an Install action, and it is not the same thing. It enrols nothing
and records no state — it fetches the ACP adapter, then re-runs the same probe
every other row runs. Afterward the harness is usable for exactly the reason
it always was: something is installed and signed in on this machine.

---

## Why synthesis is demand-driven

`effective_harnesses()` is **unchanged** — it still returns only what the
manifest declares. Implicit locals are synthesized in exactly two places:

1. `CompanyManifest::harness_for()` — so a binding resolves.
2. `lanes::build()` — a lane per id **some agent actually references**
   (`referenced_implicit_locals`, reading both roster halves).

This is load-bearing rather than tidy. `HarnessBrain::run_turn` returns the
plain engine when `lanes` *and* `unavailable` are both empty:

```rust
if self.lanes.is_empty() && self.unavailable.is_empty() { return engine; }
```

Folding three CLIs into every company's `effective_harnesses()` would take
**every company in every deployment** off that path — and on a server build
(no ACP factory) leave three `unavailable` entries behind as well. A company
that binds nobody to a CLI must synthesize nothing, and
`an_unreferenced_coding_cli_synthesizes_no_lane` pins that.

`GET {scope}/harnesses` lists them regardless, because a picker has to offer
an id before anyone can bind to it — but listing is not lane-building.

---

## Readiness is settled by running the adapter, not by looking for it

`PATH` presence is not the question anyone is asking. A binary can be there
and unusable — wrong architecture, missing execute bit, dangling symlink,
half-finished `npm` install, a protocol version this client no longer speaks —
and every one of those reads as available from a `PATH` walk, then fails on
the first real turn, far from its cause. Sign-in it cannot see at all: Claude
Code on macOS keeps credentials in the login Keychain, and an earlier version
of this code guessed at `~/.claude/.credentials.json` and reported working
installs as signed out.

So the subprocess decides, and `PATH` decides nothing:

| Phase | Call | Cost | Answers |
|---|---|---|---|
| 1 | `survey()` / `oc_acp_harnesses` | nothing — no lookup, no process | **`Checking`**, always |
| 2 | `confirm()` / `oc_acp_confirm_harness` | one subprocess, `initialize` + `session/new` | everything else |

Phase 1 deliberately answers nothing. `HarnessStatus` has no `path` field for
the same reason: a value that could only ever be absent reads, to the next
person, as though the list knows where things are.

`PATH` retains exactly one job, in `diagnose_absent`, reached only *after* the
OS has reported the adapter does not exist. It chooses the wording — "install
Claude Code" versus "you have Claude Code, it needs an add-on" — and never the
verdict. That distinction cannot come from the spawn, which only ever tried
the adapter.

`Checking` is pending, not a verdict. `isUsableHere` is false for it, and the
footer withholds its "N of M can run a turn" count while any row is still
checking, because that sentence is a claim.

Fired per row and in parallel, so the list paints immediately and one slow CLI
delays only its own row. Each `load` carries a generation counter; an answer
from a superseded run is dropped, so pressing "Check again" mid-probe cannot
let a stale verdict land on the newer list.

`confirm()` runs phase 1's reasoning itself rather than trusting callers to
filter — the harnesses pane confirms only `Checking` rows, but the agent
editor's model picker calls straight through on whatever harness was selected.
Spawning something already resolved would replace a specific instruction with
`No such file or directory (os error 2)`, mislabelled as a *broken* install
rather than an absent one.

### Where phase 2 stops

At `session/new`, never `session/prompt` — the call that runs inference and
bills. Reaching `session/new` buys two things `initialize` alone cannot: the
model list (the only place an adapter advertises what it can run) and a live
credential check, since a stale token passes every file-shaped test. Measured
under two seconds for both adapters.

The probe spawns with a handler that refuses every file and permission call:
it runs no turn, so any such call would mean the agent did something the
handshake does not license. The subprocess is killed on drop, whether the
probe succeeded, failed, or timed out (`CONFIRM_TIMEOUT`, 20s).

---

## The adapters are this app's dependency, so this app installs them

Somebody installs *Claude Code*. They do not install
`@agentclientprotocol/claude-agent-acp`, have no reason to know it exists, and
read "not found" as this app failing to see software they use daily. The
adapter is what makes a CLI speak this protocol — ours to provide.

`acp::tools` owns them, under `<app data dir>/acp-tools`, installed with `npm
install --prefix`. Never `npm -g`: that writes into the operator's own prefix,
can need `sudo`, and leaves packages behind this app put there unannounced.

**Explicit, never automatic.** It is a network fetch that writes executables.
The console offers a button; `oc_acp_install_harness` is what the button calls.

**Pinned, and only ours is policed.** Each `Harness` names a version this build
installs and checks. An adapter the operator installed globally is theirs —
`resolve_adapter` prefers ours and falls back to `PATH`, but never calls
theirs outdated. `block/buzz` does version-police, by *running* the adapter
under a restricted `PATH`; with no `node` findable the shim failed, and a
failed execution was reported as "adapter outdated — reinstall required"
(their #2342). Reinstalling never helped. This reads the version out of the
package's own `package.json`, which cannot fail that way.

**Node stays a prerequisite, and says so.** Both adapters are
`#!/usr/bin/env node` scripts, so installing one does not make it runnable.
`NodeMissing` is its own state, checked before the adapter is blamed, and it
carries no Install button — offering one would fetch something that still
could not start.

The two harnesses are not symmetric, and the states reflect it:

| | brings its CLI | why |
|---|---|---|
| `codex` | yes | `codex-acp` depends on `@openai/codex` |
| `claude` | no | the Claude SDK declares no binary; the adapter resolves `claude` off `PATH` via `node-which` |

So for Claude Code the CLI is a real prerequisite, and the login shell's
`PATH` matters twice: once for this app to find the adapter, once for the
adapter to find its CLI. Both go through `acp::shell_env`, which resolves the
operator's shell `PATH` rather than the minimal one `launchd` hands a
Finder-launched app — without it, every harness reports missing on a machine
where all of them work.

---

## What the page must not conflate

`readiness: undefined` is **not** `notInstalled`. Nothing probed and nothing
found are different facts, and collapsing them tells someone to install a CLI
already sitting on their machine. `undefined` arises in three ways, all
rendered as "can't say from here":

- a browser, where no local probe exists;
- a desktop shell predating `oc_acp_harnesses`;
- a harness that is not a local CLI — `built_in` (no CLI at all) or
  `transport = "runner"` (a CLI on somebody else's machine). Probing either
  against this machine's `PATH` would be a category error.

Sign-in is probed by **credential file**, not by launching the CLI — see
`acp::discovery`'s module docs. It can be wrong in one direction (a stale
credential reads as signed in) and that is the acceptable one: the failure
then surfaces on first use carrying the harness's own message.

---

## Deliberately out of scope

`src/runner/` — registry, Ed25519 attestation, `RunnerDispatch` — is written
and unit-tested but **wired to nothing**: not a field on `AppState`, no route,
and `lanes.rs` still records `transport = "runner"` as unavailable.

It answers a question this product does not currently ask: *other people's
machines*. Today there is one machine, the operator's own, and the ephemeral
in-memory presence map that lane is built around has no user. Reviving it
would need a dial-out endpoint, a nonce cache, a desktop runner client, and a
durable enrolment record — see this file's history for the full shape.

---

## Planned: folding this page into the agent Harness & Model picker

An implementation brief at `docs/issues/harness-provider-unification/`
proposes surfacing this page's readiness data inline in the agent-level
Harness & Model editor (`AgentDetailView.tsx`), with this page becoming a
"Manage" detail view reached from there rather than its own top-level
Settings entry. Not yet implemented — see that brief for the full design.
