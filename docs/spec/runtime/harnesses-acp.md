# ACP transports

How a `kind = "acp"` harness actually reaches an agent: the two transports, the
readiness states the desktop probes, how a teammate's conversation survives a
restart, and how its execution state reaches the console while the turn is
still running.

Split out of [harnesses.md](harnesses.md), which stays the page for what a
harness *is* — declaring them, binding agents, routing a turn, and what a
harness does not decide. Start there.

A harness declares one or the other, never both — so they are two examples,
not one block to paste:

```toml
[harness.acp]
transport = "local"      # spawn an agent on this machine
agent     = "claude"     # claude | codex
```

```toml
[harness.acp]
transport = "runner"     # reach one that dialed in
runner    = "stevens_laptop"
```

**A remote runner is a transport, not a third kind.** `transport = "local"` and
`transport = "runner"` resolve to the same `AcpAgent` port
(`crate::ports::acp::AcpAgent`); only how bytes reach the agent differs.
Modelling the runner as a third kind would add a resolution path that resolves
to the same place.

The transports differ in where they live, which is why `AcpAgent` is a **port**
rather than an ACP client in the host crate: a subprocess over stdio belongs to
the desktop shell, a WebSocket to the runner lane. The same inversion the
storage ports use — and, concretely, why the port itself lives at
`crate::ports::acp`, ungated, rather than under `crate::harness` (behind
`openhuman`): the desktop shell that supplies the `local` implementation does
not enable that feature. See that module's own docs for the full reasoning.

`local` has a real implementation as of issue #1245 — `LocalAcpAgent`
(`src-tauri/src/acp/local_agent.rs`), wired through `AppState::with_acp_agents`
and `desktop::register`. `runner` does not yet: `src/runner/dispatch.rs`
implements `AcpAgent` on `RunnerDispatch`, but nothing wires it into
`lanes::build`, so a `runner`-transport harness resolves `unavailable` on
every build today.

## One conversation per teammate, and it survives a restart

A session is per **(company, agent)** — `AcpRunTurn::session_key` is
`"{company}::{agent_id}"` — so two desks never share a conversation and the
second question in a thread does not arrive with no memory of the first. That
key is deliberately *not* per chat thread: a teammate is one correspondent, the
same way a `built_in` teammate is one live agent object however many threads it
answers on.

`LocalAcpAgent` maps that key to an ACP `sessionId` in memory, and **writes it
down**: `<workspace_root>/<company>/<agent>/acp-session.json`, beside the
agent's workspace rather than inside it (`workspace/` is the `cwd` the adapter
is given, and a file dropped in there is one the teammate can list, read and
edit).

On the first turn a process runs for a teammate, the agent tries
`session/load` before `session/new`. Without this, every runtime rebuild —
a manifest edit, an inference-settings change, an app restart — silently
started the teammate's conversation over: `LocalAcpAgentFactory::build` mints a
fresh agent with an empty session map, and nothing on the operator's screen
said the memory had gone.

Four things make the attempt safe to make unconditionally:

- It is **capability-gated** on `agentCapabilities.loadSession` from the
  adapter's own `initialize` response, read at spawn. Both catalogue adapters
  answer `true` today, and both were driven end to end rather than taken at
  their word: a codeword planted before a full process kill comes back after
  it on `claude-agent-acp` 0.70.0 *and* on `codex-acp` 1.6.2. An adapter that
  answers `false` gets a fresh session instead of a "method not found".
- A record naming a **different harness** is ignored, not tried: a Claude
  session id means nothing to `codex-acp`, and the record outlives a teammate
  being rebound between them.
- **Every failure falls through to `session/new`** — *any* failure, matched on
  nothing. That generality is load-bearing rather than lazy: the two adapters
  refuse the same situation with different codes. `claude-agent-acp` answers
  `Resource not found` (`-32002`); `codex-acp` answers `Internal error`
  (`-32603`, `data.details = "no rollout found for thread id …"`). Matching on
  a code would have left one of them hard-failing an operator's turn.
- **The record is kept when a load fails, not dropped.** `Gone` (the adapter
  exited) and `Io` (a failed stdio write) are failures of the *transport*, and
  neither says the session is invalid — while the `session/new` that follows
  is about to fail against that same dead client, which would leave the next
  start with nothing to resume. So nothing deletes the record; a successful
  `session/new` overwrites it. A session that really is gone is therefore
  replaced rather than retried forever, and the cost of a genuinely dead
  record is one refused load per cold start, only while every `session/new` is
  also failing.
- **A session is not loadable until it has completed a turn.** Both adapters
  refuse an id that was minted by `session/new` and never prompted — there is
  no rollout to replay yet. The record is written at `session/new` regardless,
  so a process that dies before its teammate's first turn costs the next start
  one refused round trip and then a fresh session. That is the fallback
  working, not a case to special-case.
- The **replay is not the turn.** `session/load` replays the conversation as
  ordinary `session/update` notifications, including the operator's own past
  messages (`user_message_chunk`, which `parse_update` maps to nothing).
  `prompt` clears the session's buffer after the load and registers its live
  observer only afterwards, so replayed history reaches neither the folded
  timeline nor a watching console.

Model steering runs against whichever session results, so an operator who
changed the model between runs is not left talking to the old one. Confirmed
live on both adapters: a `session/load` response carries the same
`configOptions` entry with `category: "model"` that `session/new` does
(`currentValue` `default` on `claude-agent-acp`, `gpt-5.6-sol` on
`codex-acp`), which is exactly what `model_config_id` reads. It stays
best-effort in code — an adapter whose load response advertises no model
option leaves the resumed session on what it was created with, which is a
working teammate rather than a broken one — but on what ships today it
actually applies.

## Execution state, before the result

An ACP turn publishes its tool calls onto the transient turn-stream bus
(`src/turn_stream.rs`) **as they happen**, the same bus and the same frame
shapes the built-in harness's collector uses — so the console renders an ACP
teammate's timeline live, with no frontend change to tell the two engines
apart.

It did not, until now: `session/prompt` buffered every update and handed back
one `AcpTurn` at the end, so an ACP-run teammate sat silent for the whole turn
and then produced a finished timeline. On a five-minute coding turn that is
indistinguishable from a hang — beside a `built_in` teammate that shows each
call as it starts.

The seam is an optional observer on the port (`ports::acp::AcpObserver`),
passed into `AcpAgent::prompt` and called by the transport as each
`session/update` arrives. A tee, never a hand-off: the transport still buffers
everything, `fold` still reads the buffer, and the live view is therefore the
same events read twice rather than a second derivation that could drift. A
dropped live frame (a lagging console) is cosmetic.

Which surface a turn streams to follows the same rules as `built_in`:

| turn | streams to |
|---|---|
| chat (`run` / `run_steered`) | the frame's own `chatId`, falling back to the default desk |
| dispatched card (`run_steered_background`) | nothing — its steps fold into the card's note |
| workflow node (`run_background_workflow`) | the run-trace sheet, by `workflowRunId`/`nodeId` |

`run_background`/`run_background_workflow` are **overridden** rather than
inherited for exactly this reason: the trait default forwards to `run` with no
chat id, which would now publish a workflow node's tool calls onto the default
desk's chat timeline.

Two things do not stream. Assistant text adds no live row (the reply is the
bubble body, and nothing on this bus carries the text itself), and a
non-terminal `tool_call_update` (`pending`/`in_progress`) publishes nothing —
its row is already on screen as `running`, which is also exactly what `fold`
leaves the step as.

A tool call's `title` and result summary are **bounded by the transport**
(`MAX_TITLE_CHARS` / `MAX_RESULT_CHARS` in `local_agent.rs`) before either
view sees them. Unlike a built-in step's server-computed label, these are
unvalidated text from an external agent process, and they now reach two places
at once — the durable step and every watching console.

`transport = "runner"` streams nothing: its wire hands back a whole `AcpTurn`
when the remote turn is over, so there is no per-update stream on this side to
tee. Making it live is a change to the runner wire, not to this fold.

**Still missing: the durable run trace.** `AcpRunTurn` ignores the
`RunTraceSink` every `RunTurn` method offers it, so a *dispatched card* run by
an ACP teammate mints its attempt row and persists no steps under it — its
timeline exists only in the card's note, written at the end. The live bus
above does not close that gap: it is deliberately ephemeral and journal-less.
The blocker is shape, not plumbing — `RunTraceSink::record` takes an
`oh::AgentProgress`, which is what the built-in collector has and an ACP fold
does not; giving the sink a `TurnStep`-shaped entry point means owning step
ordinals and the running→finalized rewrite from a second producer.

## Readiness

For `transport = "local"`, the desktop probes four states rather than two:

| state | what to do |
|---|---|
| `NotInstalled` | install it |
| `NotSignedIn` | sign in |
| `Ready` | — |
| `SpawnFailed` | read the reason |

**Installed but not signed in** is the most common state on a fresh machine, and
it looks identical to "not installed" if all you check is `which`. The fixes are
completely different, so collapsing them tells someone to do the wrong thing.

Sign-in is probed by looking for the harness's credential file, not by running
it: asking a harness whether it is logged in means starting it, which is slow on
a list refreshed whenever a settings pane opens, and for some prompts
interactively. The probe can be wrong in one direction — a stale credential
reads as signed in — and that is the acceptable direction, because the failure
then surfaces on first use with the harness's own message, which is more
accurate than anything guessed.
