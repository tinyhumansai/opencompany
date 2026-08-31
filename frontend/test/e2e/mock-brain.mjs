#!/usr/bin/env node
//
// The mock inference backend the live-brain end-to-end lane runs against
// (issue #467).
//
// Four of the suite's specs need an agent that actually executes, which needs
// a host built with `--features openhuman,mcp` **and** something for
// that harness to think with. This is that something: an OpenAI-compatible
// chat-completions endpoint with no model behind it, whose answers are very
// nearly a function of the prompt.
//
// **Very nearly, not purely** — worth knowing before you add a caller.
// `servedDirectives` is per-process, so a `__MOCK_TOOL_CALL__` fires for the
// FIRST request that carries it and never again. Any second call that sees the
// same operator message therefore changes what the first one gets. Issue #678
// hit exactly that: a triage escalation is handed the operator's raw message,
// so it carried the directive, burned it, and left the agent's own turn with a
// plain text reply — the tool call was logged once, for the classification.
// `isTriageRequest` is why that no longer happens.
//
// `wiring.spec.ts`'s header has described it since the
// day it was written ("a mocked inference backend that echoes a `__MOCK_LLM__`
// marker"); until now nobody had committed one, so the specs it describes were
// skipped rather than run.
//
// # Why a mock and not a real provider
//
// A real backend would make the suite depend on a credential, a network, and a
// model's mood. The specs behind `PW_LIVE_BRAIN` do not assert anything
// about the quality of a reply — they assert that the chain *runs*: session →
// console → `POST /company/chat` → harness → inference → tool call → board card
// → journal → rendered bubble. Every link in that chain is real here. Only the
// cognition is scripted, because scripted cognition is the only kind a test can
// assert on.
//
// # The wire format
//
// `src/harness/provider.rs`'s `HostedProvider` POSTs to
// `{base_url}/chat/completions` and parses `choices[0].message.{content,
// tool_calls}` plus `choices[0].finish_reason` — plain OpenAI. The host's
// embeddings client (`src/harness/embeddings.rs`) shares the same base URL and
// POSTs to `{base_url}/embeddings`, and it *validates* the returned width, so
// `/embeddings` is served here too rather than left to 404 in the middle of a
// memory write.
//
// # The arms, in the order they are tried
//
// Everything this server does is decided by scanning the request's messages.
// The order is load-bearing, not incidental: each of the first two arms exists
// because a later arm would otherwise consume a directive that was not meant
// for it.
//
//   1. a **triage classification** (issue #678) — answer `chatter` and touch
//      nothing else. It is handed the operator's raw message, so it carries any
//      directive that message carried, and serving one here burns it.
//   2. the host's **re-issue instruction** as the last message (issue #820) —
//      emit the named call with the arguments the instruction dictates. The
//      directive that produced the parked call has already been served, so
//      without this arm no approval-gated tool can run in this lane at all.
//   3. a message carrying `__MOCK_PLAN__ [[{…},{…}],[…]]` — a whole scripted
//      turn: several calls in one assistant message, and several steps across
//      one turn's tool loop. `orchestration-simulation.spec.ts` drives a goal
//      to completion with it. Served only when the request's own `tools` carry
//      every name the step uses, which is what stops a teammate reading the
//      operator's message second-hand from honouring the orchestrator's plan.
//   4. a message carrying `__MOCK_TOOL_CALL__ {"name":…,"arguments":{…}}` —
//      emit exactly that tool call, once. `mcp.spec.ts` uses it to make an
//      agent call a named MCP tool without a model that might decide not to.
//   5. a message carrying `SPAWNONE` — call `spawn_task` once, which is what
//      `chat-to-card.spec.ts` needs an orchestrator to do.
//   6. anything else — a fixed line carrying the `__MOCK_LLM__` marker.
//
// # Why the plain reply quotes nothing
//
// It was worth trying: `EchoBrain` answers `You said: <text>`, and three specs
// in `chat-live-events` find the reply to *their* turn by that string, so a
// mock that mirrored the shape would let one spec hold against both brains.
// It does not work, and the reason is worth writing down. What arrives as the
// last user message is not what the operator typed — the harness wraps it, and
// not only with the `## Task` preamble `memory_loop::inject` adds — so
// `You said: <that>` never contains `You said: <marker>`. Quoting a prompt this
// server does not define the shape of is guesswork, so it quotes nothing, and
// those three specs skip in this lane instead (they say why).
//
// A fixed reply is also the safer neighbour: a spec that locates the operator's
// own bubble by its text cannot match the reply as well.
//
// # Why "once" is load-bearing, and what counts as served
//
// The harness sends the whole thread history on every turn, so a directive an
// earlier turn already served is still in the transcript on the next one.
// Re-firing it opens a second card per message forever — and worse, it loops
// *within* one turn: the model is called again as soon as the tool returns, and
// the directive is still right there in the history.
//
// So a directive counts as served when a tool result, or an assistant turn
// carrying tool calls, appears after it. A tool result is not always a `tool`
// message: this host drives the harness through OpenHuman's dispatcher, whose
// `to_provider_messages` renders one as a **user** message reading
// `[Tool results]\n<tool_result id="…">…</tool_result>`. Both shapes count
// (`isToolOutput`). Recognising only the native one is what made the first run
// of this lane call `spawn_task` four times for one message.
//
// When the last message is a tool result, the reply quotes it, because
// `mcp.spec.ts` asserts the remote tool's output reached the agent and the
// agent's bubble is where an operator can see it.
//
// # Running it
//
// `playwright.config.ts` starts this as a `webServer` when `PW_LIVE_BRAIN=1`
// and it is managing the host, so `npm run e2e:live` is the whole command. It
// is a standalone script with no dependencies for the other case: if you
// brought your own host with `PW_BASE_URL`, run
//
//     node frontend/test/e2e/mock-brain.mjs --bind 127.0.0.1:8099
//
// and point that host's `OPENCOMPANY_INFERENCE_URL` at `…:8099/v1` with any
// non-empty `OPENCOMPANY_INFERENCE_KEY` (nothing here checks the bearer; the
// host needs one only because a credential is what makes it choose a live
// harness over the offline echo brain).
//
// Usage:
//   node mock-brain.mjs [--bind HOST:PORT]
// Env:
//   MOCK_BRAIN_BIND   same as --bind (default 127.0.0.1:8099)
//
// A `:0` port binds an ephemeral one; the chosen address is always printed to
// stderr as `[mock brain] listening on http://HOST:PORT`, which is how
// `test/unit/mock-brain.test.ts` finds the server it just spawned.

import { createServer } from "node:http";

import { embeddings } from "./embedding.mjs";

/** The marker every text reply carries, so a spec can prove the reply is ours. */
const MARKER = "__MOCK_LLM__";

/** Prefix of the "call exactly this tool" directive, followed by a JSON object. */
const TOOL_CALL_DIRECTIVE = "__MOCK_TOOL_CALL__";

/**
 * "Take this long to answer", followed by milliseconds — e.g.
 * `__MOCK_SLOW_MS__ 1500` (issue #863).
 *
 * Every other reply here is immediate, which is the right default and also why
 * a whole class of behaviour was untestable: a workflow whose nodes each answer
 * in a millisecond finishes before a spec can observe the run in flight at all.
 * A spec that needs to watch a run WHILE it walks the graph — the live canvas —
 * puts this in the run request, and the agent nodes downstream inherit it.
 *
 * Read off the message text rather than an environment variable on purpose: the
 * mock brain is started once for the whole suite by `playwright.config.ts`, so
 * an env knob would be a property of the lane and not of the spec that needs it.
 */
const SLOW_DIRECTIVE = "__MOCK_SLOW_MS__";

/** Milliseconds the reply should be held back, read off the directive above. */
function slowMillis(messages) {
  for (const message of messages) {
    const content = typeof message?.content === "string" ? message.content : "";
    const at = content.indexOf(SLOW_DIRECTIVE);
    if (at === -1) continue;
    const ms = Number.parseInt(content.slice(at + SLOW_DIRECTIVE.length).trim(), 10);
    // A cap, because this runs inside a suite with real timeouts: a typo'd
    // directive must slow one reply down, never wedge the lane.
    if (Number.isFinite(ms) && ms > 0) return Math.min(ms, 10_000);
  }
  return 0;
}

/** The cue that makes the orchestrator open exactly one board card. */
const SPAWN_DIRECTIVE = "SPAWNONE";

/**
 * A whole scripted **turn**, for the orchestration simulation (this is what
 * `orchestration-simulation.spec.ts` drives).
 *
 * `SPAWNONE` and `__MOCK_TOOL_CALL__` can each buy exactly one tool call from
 * one turn, which is enough to prove a chat message reaches a card and no more.
 * An orchestrator pursuing a goal does something they cannot express: it fans
 * out to several teammates in **one** turn, and then keeps going across the
 * several turns a goal takes to close. So this directive carries a plan —
 *
 *   __MOCK_PLAN__ [[{"name":"spawn_task","arguments":{…}},
 *                   {"name":"spawn_task","arguments":{…}}],
 *                  []]
 *
 * — an array of steps, each step an array of calls to emit together in one
 * assistant message. Step *n* answers the *n*th time this plan is asked for;
 * past the end (or on an empty step) the turn falls through to the ordinary
 * text reply, which is how a turn *ends* rather than looping forever.
 *
 * # Two things it must not do, and how each is prevented
 *
 * **It must not fire for an agent that cannot make the call.** One operator
 * message reaches the orchestrator and then every teammate the turn hands work
 * to, each inside its own wrapper — so a plan naming `spawn_task` is in the
 * *engineer's* prompt too, and the engineer has no such tool. A step is
 * therefore served only when every tool it names is on the belt the request
 * actually carries ({@link offeredTools}); otherwise it is left alone,
 * unconsumed, and the teammate answers with prose like any other turn. This is
 * a check on the request rather than on the prompt's wording, which is the only
 * form of it that cannot be fooled by a re-wrapping.
 *
 * **A step must not be served twice.** `spawn_task` is serviced by the
 * runtime's delegation seam rather than the agent's own tool loop, so its
 * result never enters the model-visible transcript — the history looks
 * untouched on the next call of the same turn (the same trap
 * {@link servedDirectives} exists for). Progress is therefore counted here,
 * per plan, in {@link servedPlans}, keyed by the directive and the rest of its
 * line. So a spec writes its `Date.now()` marker AFTER the payload —
 * `__MOCK_PLAN__ [[…]] goal-1787…` — and two plans holding identical steps
 * stay two plans rather than sharing one cursor.
 */
const PLAN_DIRECTIVE = "__MOCK_PLAN__";

/**
 * How many steps of each plan have been served, keyed by the plan's own text.
 *
 * @type {Map<string, number>}
 */
const servedPlans = new Map();

/**
 * The host's own re-issue instruction, sent to the agent when an operator
 * approves a parked tool call (`src/harness/brain.rs`):
 *
 *   Operator approved your `composio_execute` call. Re-issue it now with
 *   EXACTLY these arguments: {…}. Do not modify them.
 *
 * Honouring it is not a fourth directive — it is the same behaviour a real
 * model has on that prompt, and without it **no approval-gated tool can ever
 * run in this lane**. The directive arms fire once per identity, so on the
 * re-issue turn the original `__MOCK_TOOL_CALL__` is already served and the
 * mock would answer with prose; the operator's approval would then produce a
 * cheerful reply and no call, which is exactly the failure #243 was about. Any
 * spec about an `Execute`-level tool (`composio_execute`, `repo_publish`)
 * needs this.
 *
 * The arguments are re-issued VERBATIM, as the instruction demands: the grant
 * admits one call matching them exactly, so drift would simply re-park.
 */
const REISSUE_PATTERN =
  /Operator approved your `([^`]+)` call\. Re-issue it now with EXACTLY these arguments: /;

/** How much of a tool result is quoted back in the reply that follows it. */
const TOOL_ECHO_LIMIT = 2000;

/**
 * The text of one wire message, tolerating both shapes OpenAI allows: a plain
 * string, and the content-part array. The host only ever sends the former;
 * the latter costs two lines and removes a way for this to go quietly wrong.
 *
 * @param {any} message
 * @returns {string}
 */
function textOf(message) {
  const content = message?.content;
  if (typeof content === "string") return content;
  if (Array.isArray(content)) {
    return content
      .map((part) => (typeof part?.text === "string" ? part.text : ""))
      .join(" ");
  }
  return "";
}

/**
 * Reads a complete JSON object out of `text` starting at the first `{` at or
 * after `from`, by counting braces outside of string literals.
 *
 * A regex cannot do this: the directive's payload nests (`{"name":…,
 * "arguments":{…}}`) and is followed by whatever prose the harness wrapped the
 * operator's message in, so there is no delimiter to match against — only
 * balance.
 *
 * @param {string} text
 * @param {number} from
 * @returns {any | null} the parsed value, or null if nothing balanced parses
 */
function readJsonObject(text, from) {
  return readJsonValue(text, from, "{", "}");
}

/**
 * The balance scanner both directive payloads are read with: an object for
 * `__MOCK_TOOL_CALL__`, an array for `__MOCK_PLAN__`.
 *
 * Parameterised over the delimiters rather than duplicated, because the string
 * handling is the part that has to be right and a second copy of it is a second
 * place for it to be wrong.
 *
 * @param {string} text
 * @param {number} from
 * @param {string} open
 * @param {string} close
 * @returns {any | null} the parsed value, or null if nothing balanced parses
 */
function readJsonValue(text, from, open, close) {
  const start = text.indexOf(open, from);
  if (start < 0) return null;

  let depth = 0;
  let inString = false;
  let escaped = false;
  for (let i = start; i < text.length; i += 1) {
    const ch = text[i];
    if (inString) {
      if (escaped) escaped = false;
      else if (ch === "\\") escaped = true;
      else if (ch === '"') inString = false;
      continue;
    }
    if (ch === '"') inString = true;
    else if (ch === open) depth += 1;
    else if (ch === close) {
      depth -= 1;
      if (depth === 0) {
        try {
          return JSON.parse(text.slice(start, i + 1));
        } catch {
          return null;
        }
      }
    }
  }
  return null;
}

/**
 * The line `needle` sits on, collapsed and clipped — a readable title for the
 * card `SPAWNONE` opens, so a failed run shows which message opened it.
 *
 * @param {string} text
 * @param {string} needle
 * @returns {string}
 */
function titleFrom(text, needle) {
  const at = text.indexOf(needle);
  if (at < 0) return "Mock spawned task";
  const lineStart = text.lastIndexOf("\n", at) + 1;
  const lineEnd = text.indexOf("\n", at);
  const line = text.slice(lineStart, lineEnd < 0 ? text.length : lineEnd);
  // The directive itself is REMOVED from the title, and that is load-bearing.
  // The runtime reports a spawned card back into the next prompt as
  // `A card titled "<title>". It will be opened on the board this turn.` — so a
  // title carrying `SPAWNONE` puts the directive back in front of the model,
  // inside a sentence that is re-wrapped and re-truncated each round. That is
  // what produced four cards for one message across the lane's first four runs,
  // with a different key every time:
  //
  //   spawn:SPAWNONE 1786015999106
  //   spawn:SPAWNONE 1786015999106". It will be opened on the board this turn.
  //   spawn:SPAWNONE 1786015999106". It will be op...". It will be opened …
  //
  // Nothing the fixture writes may contain a directive.
  const collapsed = line.split(needle).join("").replace(/\s+/g, " ").trim();
  if (!collapsed) return "Mock spawned task";
  return collapsed.length > 80 ? `${collapsed.slice(0, 77)}...` : collapsed;
}

/**
 * The last directive in the thread, or null. Returns its position and an
 * identity: position answers "has a tool run since", identity answers "have I
 * already served this exact one".
 *
 * @param {any[]} messages
 * @returns {{index: number, id: string, name: string, arguments: any} | null}
 */
function findDirective(messages) {
  for (let i = messages.length - 1; i >= 0; i -= 1) {
    const text = textOf(messages[i]);
    // Context augmentation can quote a prior directive in a truncated task
    // summary before the current complete directive. Search from the end so
    // the newest valid instruction wins; a malformed historical quote must not
    // prevent the fixture from serving the operator's actual message.
    let at = text.lastIndexOf(TOOL_CALL_DIRECTIVE);
    while (at >= 0) {
      const payload = readJsonObject(text, at + TOOL_CALL_DIRECTIVE.length);
      if (payload && typeof payload.name === "string") {
        return {
          index: i,
          id: JSON.stringify(payload),
          name: payload.name,
          arguments: payload.arguments ?? {},
        };
      }
      at = text.lastIndexOf(TOOL_CALL_DIRECTIVE, at - 1);
    }
    const spawnAt = text.indexOf(SPAWN_DIRECTIVE);
    if (spawnAt >= 0) {
      // Identity is the directive and what follows it on its line — NOT the
      // whole line, and not the message. One operator message reaches several
      // agents (the orchestrator, then each desk the turn delegates to), each
      // inside its own wrapper, so a key that includes the prefix differs per
      // agent and every one of them honours the directive again. That is the
      // second cause of the four cards for one message, and the one the history
      // check and the whole-line key both missed.
      const id = `spawn:${text.slice(spawnAt).split("\n")[0].trim()}`;
      return {
        index: i,
        id,
        name: "spawn_task",
        arguments: { title: titleFrom(text, SPAWN_DIRECTIVE) },
      };
    }
  }
  return null;
}

/**
 * The last {@link PLAN_DIRECTIVE} in the thread, or null.
 *
 * Scanned from the end so the newest plan wins: a goal takes several operator
 * messages to close, and each of them carries the plan for the turn it opens.
 *
 * @param {any[]} messages
 * @returns {{id: string, steps: any[][]} | null}
 */
function findPlan(messages) {
  for (let i = messages.length - 1; i >= 0; i -= 1) {
    const text = textOf(messages[i]);
    const at = text.indexOf(PLAN_DIRECTIVE);
    if (at < 0) continue;
    const steps = readJsonValue(text, at + PLAN_DIRECTIVE.length, "[", "]");
    if (!Array.isArray(steps)) {
      // A broken spec, not a plain turn. Say so loudly rather than answering
      // with text the spec will then fail on obscurely.
      process.stderr.write(
        `[mock brain] ${PLAN_DIRECTIVE} found but its JSON payload did not parse\n`,
      );
      return null;
    }
    // Identity is the directive and the rest of its LINE — the same key shape
    // `SPAWNONE` uses, and for both of its reasons. It is stable across the
    // wrappers each agent's prompt puts in FRONT of the message, and it keeps
    // two plans that happen to hold identical steps apart, because the marker a
    // spec writes after the payload is part of the key. Keying on the parsed
    // steps alone would have made "the same two cards, opened by a later goal"
    // share a cursor with the first goal and silently start half way through.
    const line = text.slice(at).split("\n")[0].trim();
    return { id: `plan:${line}`, steps };
  }
  return null;
}

/**
 * The tool names this request actually offers, which is what tells an
 * orchestrator turn from a teammate's turn on the same operator message.
 *
 * The orchestrator's belt really does carry `spawn_task`, `review_task` and the
 * rest on the wire — 27 tools on the harness company, against the teammate's
 * fourteen — so this is a sufficient check and not a heuristic. It was worth
 * confirming rather than assuming: the delegation tools are *intrinsic*, in the
 * sense that the runtime services them rather than the agent's own tool loop,
 * and it would have been reasonable to guess they were therefore absent from
 * `tools[]`.
 *
 * @param {any} body the parsed request
 * @returns {Set<string>}
 */
function offeredTools(body) {
  const tools = Array.isArray(body?.tools) ? body.tools : [];
  return new Set(
    tools
      .map((tool) => tool?.function?.name ?? tool?.name)
      .filter((name) => typeof name === "string"),
  );
}

/**
 * The host's re-issue instruction in the last message, or null.
 *
 * Only the last message is considered. An instruction further back was already
 * answered on the turn it arrived, and re-answering it would call the tool
 * again every turn for the rest of the thread.
 *
 * @param {any[]} messages
 * @returns {{name: string, arguments: any} | null}
 */
function findReissue(messages) {
  const text = textOf(messages[messages.length - 1]);
  const match = REISSUE_PATTERN.exec(text);
  if (!match) return null;
  const args = readJsonObject(text, match.index + match[0].length);
  if (!args) {
    process.stderr.write("[mock brain] re-issue instruction found but its arguments did not parse\n");
    return null;
  }
  return { name: match[1], arguments: args };
}

/**
 * Directive identities already acted on, for the life of this process.
 *
 * The history check below is the honest one and covers the common case, but it
 * cannot cover every one: a tool whose result never reaches the model-visible
 * transcript — `spawn_task` is serviced by the runtime's delegation seam, not
 * by the agent's own tool loop — leaves a history that looks untouched, so the
 * directive fires again on the next call of the same turn, and again, until the
 * loop hits its cap. The lane's first two runs opened four cards for one
 * message that way. Identity is what makes "once" hold regardless: every
 * directive a spec writes carries a `Date.now()` marker, so a genuinely new one
 * is always a new key.
 *
 * @type {Set<string>}
 */
const servedDirectives = new Set();

/**
 * Whether a message carries the output of a tool that ran.
 *
 * Two shapes, because two are produced. A provider-native transcript puts it in
 * a `tool` message; OpenHuman's dispatcher — which is what this host drives —
 * renders the same thing as a **user** message reading `[Tool results]` with
 * `<tool_result id="…">` blocks inside. Missing the second shape means the mock
 * never sees its own tool call come back.
 *
 * @param {any} message
 * @returns {boolean}
 */
function isToolOutput(message) {
  if (message?.role === "tool") return true;
  const text = textOf(message);
  return text.includes("[Tool results]") || text.includes("<tool_result");
}

/**
 * The readable part of a tool result: the text inside the `<tool_result>`
 * wrappers, or the whole message when it carries none.
 *
 * @param {any} message
 * @returns {string}
 */
function toolOutputText(message) {
  const text = textOf(message);
  const inner = [...text.matchAll(/<tool_result[^>]*>([\s\S]*?)<\/tool_result>/g)]
    .map((match) => match[1].trim())
    .filter(Boolean);
  return (inner.length ? inner.join("\n") : text.replace("[Tool results]", "")).trim();
}

/**
 * Whether the directive at `index` has already been acted on in this thread:
 * a tool result, or an assistant turn carrying tool calls, after it.
 *
 * @param {any[]} messages
 * @param {number} index
 * @returns {boolean}
 */
function alreadyServed(messages, index) {
  return messages.slice(index + 1).some((message) => {
    if (isToolOutput(message)) return true;
    return (
      message?.role === "assistant" &&
      Array.isArray(message?.tool_calls) &&
      message.tool_calls.length > 0
    );
  });
}

/**
 * The reply body for one chat-completions request.
 *
 * @param {any} body the parsed request
 * @returns {any} an OpenAI-shaped chat completion
 */
/**
 * Whether this request is a triage escalation rather than an agent turn
 * (issue #678).
 *
 * Keyed on the opening sentence of the system prompt that
 * `src/harness/triage.rs` owns. Coupling a fixture to prose is ordinarily a
 * smell; the alternative here is worse, because the only other thing telling
 * the two apart is "carries no tools", and an agent whose belt happens to be
 * empty would be misread as a classification.
 *
 * @param {any[]} messages
 * @returns {boolean}
 */
function isTriageRequest(messages) {
  const first = messages[0];
  return typeof textOf(first) === "string" && textOf(first).includes("You classify one message");
}

/**
 * A planning pass (issue #337), recognised by its own system prompt.
 *
 * Like a triage classification, this is not an agent turn: the pass runs with no
 * tools and expects one JSON object back. Before this arm existed a planning
 * prompt fell through to the turn arms and came back as prose, which the host
 * reads as an unparseable answer — so every card dragged into Planning in this
 * lane settled as a failed pass.
 */
function isPlanningRequest(messages) {
  const first = messages[0];
  return (
    typeof textOf(first) === "string" && textOf(first).includes("You are the planning desk")
  );
}

/**
 * The plan this lane answers every planning pass with (issue #1106).
 *
 * Deliberately **ambiguous**: it names two teammates the `e2e_harness` roster
 * really carries, so the host resolves both and the card parks asking who owns
 * it rather than dispatching. That is the whole behaviour under test, and it is
 * unreachable from a fixture that names one.
 *
 * `prerequisites` is empty on purpose. A missing prerequisite parks the card
 * too, by a different arm and with a different note — leaving one here would
 * make a passing test unable to say which mechanism it had exercised.
 */
const AMBIGUOUS_PLAN = JSON.stringify({
  description:
    "Find what is being said about the topic and write up what matters, with links.",
  steps: [
    { title: "Gather the sources", detail: "Search and collect what is current." },
    { title: "Write the digest", detail: "Summarise with links, newest first." },
  ],
  prerequisites: [],
  risks: ["the sources may be thin on the day it runs"],
  verification: "a digest exists with at least three linked sources",
  scope: "the digest only; no publishing",
  assigneeCandidates: [
    { id: "engineer", reason: "already automates the collection side of this" },
    { id: "writer", reason: "owns everything the company publishes in prose" },
  ],
});

function chatCompletion(body) {
  const messages = Array.isArray(body?.messages) ? body.messages : [];
  const model = typeof body?.model === "string" ? body.model : "mock-brain";

  // `MOCK_BRAIN_DEBUG=1` dumps what actually arrived. Three of the arms below
  // key on the *shape* of a request — the opening words of a system prompt, the
  // names in `tools[]` — and when one of them stops matching, the only useful
  // next question is what the host really sent. Guessing at that from a spec
  // failure forty seconds downstream is how an afternoon goes.
  if (process.env.MOCK_BRAIN_DEBUG === "1") {
    process.stderr.write(
      `[mock brain] REQUEST roles=[${messages.map((m) => m?.role).join(", ")}] ` +
        `tools=[${(body?.tools ?? []).map((t) => t?.function?.name).join(", ")}]\n` +
        messages
          .map((m, i) => `  [${i}] ${m?.role}: ${textOf(m).slice(0, 400).replace(/\n/g, " ")}`)
          .join("\n") +
        "\n",
    );
  }

  // A triage escalation is a classification, not a turn (issue #678). It is
  // handed the operator's RAW message, so it carries any `__MOCK_TOOL_CALL__`
  // the message carried — and `servedDirectives` is per-process, so serving it
  // here would burn the directive and leave the real turn with a plain text
  // reply. Observed exactly that way: the tool call was logged once, for the
  // classification, and the agent's own turn never made it.
  //
  // Answered `chatter` rather than refused, so the suite stays on the ungated
  // path it was written for: only an `answer` verdict narrows the delegation
  // claim.
  //
  // **First arm tried**, ahead of the re-issue arm below as well as the
  // directive arms: everything after this point assumes an agent turn, and a
  // classification is not one. It cannot currently reach the re-issue arm —
  // `findReissue` requires the host's instruction to be the LAST message and a
  // classification's last message is the operator's — but that is a property of
  // one prompt, not a rule worth relying on.
  if (isTriageRequest(messages)) {
    process.stderr.write("[mock brain] triage classification (no directive consumed)\n");
    return completion(model, { role: "assistant", content: "chatter" }, "stop");
  }

  // Beside the triage arm and for the same reason: a planning pass is not an
  // agent turn, so it must not reach the directive arms below — a card whose
  // text happened to carry `__MOCK_TOOL_CALL__` would otherwise burn it here
  // and leave the real turn with a plain reply, which is exactly the bug #678
  // fixed for triage.
  if (isPlanningRequest(messages)) {
    process.stderr.write("[mock brain] planning pass (ambiguous plan, no directive consumed)\n");
    return completion(model, { role: "assistant", content: AMBIGUOUS_PLAN }, "stop");
  }

  // Ahead of the directive arms, and only when the instruction is the LAST
  // thing said: the re-issue prompt is a fresh turn from the host, so anything
  // older in the transcript — including the directive that produced the parked
  // call — has already had its say.
  const reissue = findReissue(messages);
  if (reissue) {
    process.stderr.write(`[mock brain] re-issuing approved call: ${reissue.name}\n`);
    return completion(
      model,
      {
        role: "assistant",
        content: null,
        tool_calls: [
          {
            id: `mock-reissue-${messages.length}`,
            type: "function",
            function: {
              name: reissue.name,
              arguments: JSON.stringify(reissue.arguments),
            },
          },
        ],
      },
      "tool_calls",
    );
  }

  // The scripted-turn arm, ahead of the single-call directives: a plan is the
  // whole turn, and a message carrying one carries nothing else.
  const plan = findPlan(messages);
  if (plan) {
    const served = servedPlans.get(plan.id) ?? 0;
    const step = plan.steps[served];
    const calls = Array.isArray(step) ? step : [];
    if (calls.length > 0) {
      const offered = offeredTools(body);
      const missing = calls.map((call) => call?.name).filter((name) => !offered.has(name));
      if (missing.length > 0) {
        // NOT consumed: this is a teammate reading the operator's message
        // second-hand, not the orchestrator. Answering with prose is the same
        // thing a real model does when it is offered no such tool.
        // The belt is named as well as the gap: "this agent is not the one the
        // plan was written for" and "the agent lost a tool it should have" are
        // the same line otherwise, and they are opposite bugs.
        process.stderr.write(
          `[mock brain] plan step ${served} left unserved; this belt has no ` +
            `${missing.join(", ")} — it carries [${[...offered].join(", ")}]\n`,
        );
      } else {
        servedPlans.set(plan.id, served + 1);
        process.stderr.write(
          `[mock brain] plan step ${served}: ${calls.map((c) => c.name).join(" + ")}\n`,
        );
        return completion(
          model,
          {
            role: "assistant",
            content: null,
            tool_calls: calls.map((call, index) => ({
              id: `mock-plan-${served}-${index}`,
              type: "function",
              function: {
                name: call.name,
                arguments: JSON.stringify(call.arguments ?? {}),
              },
            })),
          },
          "tool_calls",
        );
      }
    } else if (Array.isArray(step)) {
      // An explicitly empty step: the turn is meant to answer in prose here.
      // Consumed, so the next call moves on rather than re-reading this one.
      servedPlans.set(plan.id, served + 1);
      process.stderr.write(`[mock brain] plan step ${served}: text reply\n`);
    }
  }

  const directive = findDirective(messages);

  if (
    directive &&
    !servedDirectives.has(directive.id) &&
    !alreadyServed(messages, directive.index)
  ) {
    servedDirectives.add(directive.id);
    // The id, not just the name: when a directive fires more than once the
    // question is always "which key differed", and this is the line that
    // answers it from a CI log alone.
    process.stderr.write(`[mock brain] tool call: ${directive.name} <${directive.id}>\n`);
    return completion(model, {
      role: "assistant",
      content: null,
      tool_calls: [
        {
          id: `mock-call-${directive.index}`,
          type: "function",
          function: {
            name: directive.name,
            arguments: JSON.stringify(directive.arguments),
          },
        },
      ],
    }, "tool_calls");
  }

  const last = messages[messages.length - 1];
  const content = isToolOutput(last)
    ? `${MARKER} ${toolOutputText(last).slice(0, TOOL_ECHO_LIMIT)}`
    : `${MARKER} mock inference backend reply.`;
  process.stderr.write(`[mock brain] text reply (${content.length} chars)\n`);
  return completion(model, { role: "assistant", content }, "stop");
}

/**
 * Wraps one assistant message in the completion envelope, with a zeroed usage
 * block. Zero is the honest number and it keeps the harness's cost pipeline on
 * its billing-free path, so a suite run never books spend against the company.
 *
 * @param {string} model
 * @param {any} message
 * @param {string} finishReason
 * @returns {any}
 */
function completion(model, message, finishReason) {
  return {
    id: "chatcmpl-mock",
    object: "chat.completion",
    created: 0,
    model,
    choices: [{ index: 0, message, finish_reason: finishReason }],
    usage: { prompt_tokens: 0, completion_tokens: 0, total_tokens: 0 },
  };
}

/**
 * Reads a whole request body.
 *
 * @param {import("node:http").IncomingMessage} request
 * @returns {Promise<string>}
 */
function readBody(request) {
  return new Promise((resolve, reject) => {
    /** @type {Buffer[]} */
    const chunks = [];
    request.on("data", (chunk) => chunks.push(chunk));
    request.on("end", () => resolve(Buffer.concat(chunks).toString("utf8")));
    request.on("error", reject);
  });
}

/**
 * @param {import("node:http").ServerResponse} response
 * @param {number} status
 * @param {any} payload
 */
function sendJson(response, status, payload) {
  const body = JSON.stringify(payload);
  response.writeHead(status, {
    "content-type": "application/json",
    "content-length": Buffer.byteLength(body),
  });
  response.end(body);
}

const server = createServer((request, response) => {
  const path = new URL(request.url ?? "/", "http://localhost").pathname;

  // Whatever `{base_url}` the host was given, the two routes it POSTs are
  // `…/chat/completions` and `…/embeddings`. Matching on the suffix means a
  // base URL with or without a `/v1` both work, which is one fewer way for the
  // lane's configuration and this server to disagree.
  if (path === "/healthz") {
    sendJson(response, 200, { ok: true });
    return;
  }

  if (request.method !== "POST") {
    sendJson(response, 405, { error: `${request.method} is not served here` });
    return;
  }

  void readBody(request)
    .then((raw) => {
      /** @type {any} */
      let body;
      try {
        body = raw ? JSON.parse(raw) : {};
      } catch (error) {
        sendJson(response, 400, { error: `unparseable request body: ${error}` });
        return;
      }
      if (path.endsWith("/chat/completions")) {
        // Issue #863: hold the reply back when the prompt asks for it, so a
        // spec can watch a workflow run while it is still walking the graph.
        const held = slowMillis(Array.isArray(body?.messages) ? body.messages : []);
        if (held > 0) {
          setTimeout(() => sendJson(response, 200, chatCompletion(body)), held);
          return;
        }
        sendJson(response, 200, chatCompletion(body));
      } else if (path.endsWith("/embeddings")) {
        sendJson(response, 200, embeddings(body));
      } else {
        sendJson(response, 404, { error: `no mock route for ${path}` });
      }
    })
    .catch((error) => {
      sendJson(response, 500, { error: String(error) });
    });
});

const bindArgument = process.argv.indexOf("--bind");
const bind =
  (bindArgument >= 0 ? process.argv[bindArgument + 1] : undefined) ||
  process.env.MOCK_BRAIN_BIND ||
  "127.0.0.1:8099";
const separator = bind.lastIndexOf(":");
const host = separator > 0 ? bind.slice(0, separator) : "127.0.0.1";
const port = Number(separator > 0 ? bind.slice(separator + 1) : bind);

server.on("error", (error) => {
  process.stderr.write(`[mock brain] cannot bind ${bind}: ${error}\n`);
  process.exit(1);
});

server.listen(port, host, () => {
  const address = server.address();
  const chosen = typeof address === "object" && address ? address.port : port;
  process.stderr.write(`[mock brain] listening on http://${host}:${chosen}\n`);
});
