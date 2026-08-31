# Running with no network

OpenCompany runs a company end to end with no outbound network: local
inference, a filesystem store, the embedded OpenHuman agent runtime, and local
MCP processes. This document is the configuration, and
[`tests/offline_e2e.rs`](../../../tests/offline_e2e.rs) is the proof — CI
executes it inside a network namespace with no routes, so the claim on this
page fails a build when it stops being true.

That ordering is the point of issue #579. An offline path documented and not
executed rots the first time a cloud call is added to a shared code path, and it
rots silently: nothing fails, and the page keeps asserting with authority.

## What is not local

Three things are hosted services, and no configuration makes them otherwise:

| Not local | What it is |
|---|---|
| **Medulla** | the hosted cognition brain |
| **Composio** | the hosted third-party tool broker |
| **Hub identity exchange** | the platform's cross-tenant identity service |

A green offline lane says **the offline path works**. It does not say everything
works offline. Configure none of the above and their surfaces are *absent*
rather than present-and-failing, which is the behaviour #579 asks for: an
operator should not be offered a tool that cannot run here.

Specifically, with the manifest below: no `composio` grant means Composio-backed
tools are never wired onto an agent's belt; no managed credential means the
cognition brain is not the hosted one; and nothing contacts the hub.

**Product analytics is not a fourth entry**, and the reason is worth recording
here rather than only in [analytics.md](analytics.md). It reports only from
hosted tenants, and its network client sits behind the `analytics` cargo feature
that the default build — the one this lane compiles — does not enable. The
policy, the `NullTracker` and the payload builder are still compiled; what is
absent is the **network client**, the only type that owns an HTTP client for
analytics. So no analytics request is possible from the binary that runs inside
the namespace, and this lane stays a proof rather than becoming a thing to work
around.

## The configuration

```toml
[company]
name = "Offline Co"
summary = "Proves the no-network path."

# Local inference. `ollama` resolves through the ordinary OpenAI-compatible
# path: one `base_url`, and no bearer at all when no key is set.
[inference]
provider = "ollama"
base_url = "http://localhost:11434/v1"
model = "llama3"

# No `composio`, no `search`, no `media` — a hosted surface is absent rather
# than offered and broken.
[tools]
allow = ["workspace", "files"]

[users]
admins = ["ada@example.com"]

[[agent]]
id = "ceo"
role = "Chief Executive"
tier = "orchestrator"

[[agent]]
id = "writer"
role = "Writer"
```

Run it:

```sh
ollama serve                     # or any OpenAI-compatible server on that port
opencompany serve --company path/to/company
```

The default store is the filesystem and needs nothing; `sqlite` also runs with
no server. `OPENCOMPANY_DATA_DIR` sets where state lives — see
[data-root.md](data-root.md).

## What the lane actually does

`tests/offline_e2e.rs` boots that manifest on loopback, signs in over the
magic-link flow, creates a card, drags it into In Progress, and watches an agent
work it — then finishes it. Every model call goes to a scripted
OpenAI-compatible endpoint on loopback, and the test asserts that endpoint was
actually called, so a run that never reached a model cannot pass as one that
did.

**Why a scripted endpoint rather than a real Ollama.** `provider = "ollama"`
resolves through the same code as any OpenAI-compatible endpoint, and its
`base_url` is overridable, so a loopback script exercises the real provider path
while keeping the lane fast and deterministic. What it does **not** prove is
that a real Ollama server is wire-compatible with this client — that needs a
model pull, which is slow and fails for reasons unrelated to this repository.

**Why the operator has to finish the card.** The lane drives the card to
`in_review` with no help, which is as far as an agent can take it: since the
operator decision of 2026-08-05 (recorded in `harness::lifecycle`),
**`done` is reached only by a person**, through an approving verdict. The test
then plays that person. Read the lane as "an agent does the work offline and a
human approves it offline", never as "an agent reaches Done unaided" — the
product does not permit the second, and #579's acceptance was written before the
decision that changed it.

## How "no network" is enforced

The CI step runs the compiled test binary under:

```sh
sudo unshare --net -- sh -c "ip link set lo up && exec <binary>"
```

A fresh network namespace has no routes, no DNS and no interfaces beyond the
loopback that command brings up, so an accidental cloud call **fails** rather
than passing unnoticed. The binary is compiled *before* the namespace is
entered, because compiling fetches crates; sandboxing the build would fail on
the registry and say nothing about the runtime.

`iptables -P OUTPUT DROP` was rejected because it would also cut the Actions
runner's own control connection; a container with `--network none` was rejected
because it needs an image built for the purpose and buys no extra isolation.

### The guard checks itself

`a_deliberate_outbound_call_fails_inside_the_namespace` attempts a real outbound
connection and asserts it fails. It runs only when the lane sets
`OPENCOMPANY_OFFLINE_LANE=1`, so the same file still runs on a networked laptop.

Without it this lane would be theatre: a sandbox that silently failed to apply
produces a green run indistinguishable from a working one, and "nothing dialled
out" cannot be told from "the jail was never locked".

## If you add a cloud call

The lane goes red, and it should. Either the call belongs behind a capability
that is absent offline — the Composio/Medulla shape, where the surface is not
offered at all — or it is a genuine new dependency and this page needs to say
so. Do not make the lane pass by giving the namespace a route.
