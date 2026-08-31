# The GraphQL read plane

The `/graphql` read surface and how it relates to the REST reads.

Split out of [`api.md`](api.md), which was over the repository's 500-line ceiling.

## Read plane — GraphQL (`/graphql`)

Every console **read** is served by a single async-graphql query surface at
`POST /graphql` (with a `GET /graphql` GraphiQL explorer in development). The
REST **read exceptions** are the console reads that ship over REST instead —
the two inbox `GET`s and the three workspace `GET`s, the task export, the
skill-registry browse, the agent detail `GET`, the policy read, and the
credential status `GET` — each because the console ships no GraphQL client and
that view needs a reachable read (issues #173, #177, #607, #352, #264, #562;
see the [write plane](api-write-plane.md)). The schema is query-only — REST
otherwise owns writes — and is **built once at startup** and stored on
`AppState`; each request injects its resolved `GqlAuth` principal.

The schema is rooted at a **`Company` aggregation object** so a view fetches
everything it needs in one round trip; the only top-level queries are
`companies`, `company(id)` (the sole company when `id` is omitted in
single-company mode), and `skillRegistry` (the unscoped shared library). Under
`Company` hang `team`, `chats`/`chat(id)`, `inboxes`, `tasks`, `skills`,
`workspaceTree`/`workspaceFile(id)`, `memory`, `workflows`/`workflow(id)`,
`usage`, `finances`, `connections`, `domain`, and `smtp`. The authoritative
contract is the SDL snapshot at
[`src/server/graphql/schema.graphql`](../../../src/server/graphql/schema.graphql)
(`graphql::sdl()` regenerates it). GraphQL mutations and subscriptions are out
of scope — streaming is wired over REST instead: `/chat` (below, the one
conversational surface) and the `/events` work feed
([events.md](events.md)).

- **`/chat`** enqueues an `OperatorMessage` event and streams the resulting
  cycle's channel responses over SSE. One conversational surface, one voice:
  the operator talks to the company, not to individual teammates.
- **`/chat` thread addressing is a load-bearing contract, not just routing.**
  The body's `chat` field names a desk; three behaviours follow from it, and
  the console's per-workflow copilot (issue #303) is built entirely on them,
  with no route of its own:
  1. An **unknown** thread id falls through to the orchestrator — the brain
     tries desk-lead, then roster agent, then its own responder.
  2. Replies are journaled against that thread, and the desk filter
     (`server::chat_history::owns`) matches the id **exactly**; the General
     catch-all applies only when General is the desk being *read*. So an
     addressed thread is isolated from the team's chat in both directions.
  3. `GET /chat/history?desk=<thread>` therefore replays exactly that thread.

- **A message has a durable id, and things can refer to it (issue #364).**
  `POST /chat` answers with `messageId` — the sequence position the operator's
  own message was journaled under — and stamps the same on each reply bubble.
  Two things name a message by that id:

  - The `parent` field on the `/chat` body makes the send a **thread reply**.
    It is journaled onto both the `OperatorMessage` and the replies it draws,
    so the whole exchange comes back under the same row on the next read. A
    `parent` that is not a message id is a `400`, never a silently-flattened
    thread — a reply that quietly lands in the channel reads as a lost reply.
  - `POST /chat/messages/{seq}/reactions` sets or clears **one person's** one
    reaction. `on` is explicit rather than a toggle, which is what makes a
    retry or a double tap idempotent. The target must be a chat message —
    anything else is a `404`, so the log can never hold a reaction no reader
    could render — and the emoji is bounded and refused if it carries control
    characters. Authorized through the same gate a send passes: reacting is
    writing into a transcript, so it can be neither easier nor harder than
    saying something in it.

  Both project through the shared `MessageView`, so REST and GraphQL cannot
  disagree about the shape of a thread or who reacted. Reactions are
  deliberately absent from the `/events` stream — see [events.md](events.md).

  The copilot addresses `workflow-copilot:<workflowId>` (a `:` cannot occur in
  a manifest desk id, so it can never collide with a real desk, and it does not
  appear in `GET …/desks`). Making unknown thread ids a `404`, or loosening
  `owns` to match on prefix, would break that surface — see
  [`frontend/src/api/workflow-copilot.ts`](../../../frontend/src/api/workflow-copilot.ts).

  **Thread addressing isolates transcripts. For every thread but one, it does
  not scope authority.** The thread id decides who answers and where the
  exchange is journaled; for an ordinary thread it does **not** narrow the
  responder's context or tool grants, which stay company-wide however the turn
  is addressed.

  **The copilot thread is the exception (issue #416).** A `chat` id matching
  `workflow-copilot:<workflowId>` ([`company::copilot`](../../../src/company/copilot.rs))
  makes the turn **confined**, host-side, in two places that hold independently:

  - the harness runs it on an ephemeral agent with **no tools, no company
    memory and no delegation** ([`harness::confine`](../../../src/harness/confine.rs)),
    and skips the retrieve→inject step and the memory writeback, so the turn
    answers from the message it was sent and leaves nothing behind. Every tool
    call is denied by the host with a reason, so an empty toolbelt is a
    boundary rather than an absence;
  - the `/chat` handler does not open a board card from a copilot message, so a
    question phrased as a request cannot leave work on the board. That half is
    in the default build, not behind `openhuman`.

  Confinement narrows one **turn**; it is not an authorization check and must
  not be read as one. `/chat` is already authenticated and company-scoped, so
  an operator addressing a workflow thread gains nothing they could not get by
  opening the Chat tab or calling the workflow routes directly. What the
  copilot adds is a transcript that stays out of the team's chat and an answer
  drawn from one workflow rather than from everything the company knows.

  **A copilot answer may carry a proposed edit (issue #415), and that adds no
  route and no capability.** The proposal is a fenced block in the reply text —
  a list of node/edge operations — which the *console* turns into a candidate
  graph and applies through `PUT …/workflows/{wid}` with `expectedVersion`, the
  same write the canvas editor performs, after the operator has read the diff
  and pressed Apply. The confined turn still calls nothing: it emits text, and
  a person decides. So the host needs no notion of a proposal, and a proposal
  cannot produce a graph the editor could not have produced — including the
  `409` a graph that moved underneath it earns.

  Two more consequences worth knowing before reusing the seam. A chat turn
  runs the **whole** company cycle, so every message is first classified by
  `company::task_intent::triage_message` (#267) into `Track` (an instruction —
  the route opens a `todo` card), `Answer` (a question or read — no card), or
  `Chatter` (greetings, and anything ambiguous — no card). `Answer` is also the
  only class that *gates*: the harness narrows the issue-#453 delegation claim
  to answering for that turn, so the model's own `spawn_task` / `assign_task` /
  `review_task` fail at the tool boundary with the do-not-retry refusal.
  `delegate_to_desk` is deliberately **not** refused — it is how a question the
  orchestrator cannot answer alone reaches a desk that can — so it runs the
  desk lead and relays their reply, and only its board card stands down.
  `query_company` / `run_workflow` / `read_run_output` run inline and are
  untouched throughout. The turn loses the ability to *write*, never the
  ability to answer. Ambiguity falls to `Chatter`, which neither cards nor gates: a
  missed card costs one follow-up message, a spurious card pollutes the board
  permanently. The gate is harness-only — `HostedMedullaBrain` has no
  delegation stack to gate (#176) — while the triage itself is compiled into
  every build and fronts both brains. The card half is suppressed wholesale on
  a copilot thread (#416), precisely because the seam is being reused for a
  conversation that is not a request to the company. And an
  unconfigured company answers
  `200` with the echo brain's `"You said: …"` rather than an error, so a caller
  that needs a real answer must check `cognition` from `GET {scope}/inference`
  — there is no status code to catch.

  **A bare pleasantry runs no cycle at all** (#1725). Before the brain is
  called, `CycleRunner::run_locked` asks `company::task_intent::small_talk`
  whether the batch is one operator message that is nothing but "hi" / "hello"
  / "thanks" — and if it is, answers it from the runtime with a one-line reply,
  zero steps, zero tokens and no memory trace, in the voice
  `delegation_tools::chat_responder` resolves for the addressed thread. The
  reply is journaled, routed and metered by exactly the paths a real turn's is;
  what is skipped is the thinking.

  This is narrower than `Chatter` on purpose. `small_talk` matches two subsets
  of the triage's greeting list and excludes every **acknowledgement** — "yes",
  "ok", "sure", "done" — because those are small talk in isolation and
  instructions in a conversation: *"yes"* answering a teammate's *"shall I ship
  it?"* must reach the turn that asked. It also declines a batch, an attachment,
  a message whose composer carried an explicit "one-off" or "build me the
  workflow" choice (a positive statement by the person who wrote it, which
  outranks anything read out of the words), and a workflow copilot thread,
  whose turns are confined and answered by an agent the runtime cannot speak
  as.

  Why the runtime answers this itself rather than trusting the turn to be
  cheap: on staging "hi" ran the full pipeline — memory retrieval, a tool step,
  and a long analysis belonging to a task nobody had asked about in that turn.
  The vendored OpenHuman turn re-injects an uncompleted per-thread goal on
  *every* turn (and resumes a paused one), and the pooled agent's transcript is
  keyed by agent id alone, so a prior task's fetched page is still in the
  context window. Both are properties of a turn that runs; the fix that holds
  regardless of what the vendored runtime does with its goals is not to run
  one. Scoping that pooled transcript per conversation, and clearing a settled
  thread goal, are still owed — see the issue.

  **There is a third card path, and it has the opposite default** (#984). The
  two above are the route's. When the responder is a desk member rather than
  the orchestrator — a DM, or any addressed thread — the runner also calls
  `open_direct_work_card`, which defers to `is_trackable_work` rather than to
  `triage_message`. That detector asks "is there any reason NOT to track this?"
  and so accepts everything except an empty string, a question **that names no
  recognised work verb**, and a short acknowledgement.

  The order of those rungs matters and is deliberate: a `WORK_VERBS` hit is
  checked **before** question syntax, so *"Can you write up the incident
  review?"* is tracked — it is a question in shape and a request for work in
  substance, and the substance wins. Only a question with no work verb is
  excluded. A message the documented Layer A classified as `Chatter` could
  therefore still become a card by this door, which is how a board came to hold
  78 chat lines in review.

  What closes it is the escalation verdict, which already ran and was
  discarded. On the abstention set only, the model is asked; an `Answer`
  narrows the claim as before, and a `Chatter` now stands both runner card
  paths down. It is carried separately from the answering flag because
  `Chatter` still must not withdraw the model's board tools — taking those away
  on a maybe turns a triage miss into work silently refused. The verdict can
  only **subtract** a card: it is consulted nowhere else, and `Work`,
  `Unavailable`, an unwired escalation or a non-harness build all leave the
  deterministic decision exactly as it was.
- **`/events`** is the work feed's backend: each frame is a plain-language
  rendering of an event or executed effect plus the raw payload for
  programmatic consumers. Resumable via `since` (event sequence number).

## Run observability

`Company.agentRuns(taskId:, workflowRunId:, limit:)` and `Company.agentRun(id:)`
return attempts with their step traces, and each step's `deep` half when the host
keeps one.

REST is unchanged and keeps its job: `GET {scope}/runs/{id}` is shipping, tested,
and its shape is deliberately the console's `TimelineEntry` contract. This
surface exists for the **joined** read — run → attempts → steps → detail in one
request — which over REST would be a round trip per node plus client-side
assembly.

Deep-trace bodies are deliberately a separate operator write surface. An
 authenticated administrator (or the hosting platform principal) can destroy
 them with `DELETE /api/v1/company/deep-trace` (all runs) or
`DELETE /api/v1/company/deep-trace/{runId}` (one run); the platform-scoped forms
replace `company` with `companies/{id}`. These routes return `204` and leave
the redacted run/step skeleton intact. Ordinary members are refused by the
admin scope guard.

Two shape decisions worth keeping:

- `AgentRun.stepCount` is **nullable on purpose**. `step_count` is written by the
  settle, so returning the stored `0` for a running attempt would be a lie a
  client cannot detect. Null is what tells a live reader to count `steps`.
- `deep` is a field *on* `RunStep`, not a parallel query, so the redacted and
  unredacted views of one step can never misalign.
