# Checkpoints and Approvals

Approvals are deliberate operator questions, not policy interception around
ordinary tool calls. This doc is normative.

## Current approval boundary

An agent raises a general approval with `request_approval` (`title`, yes/no
`question`, optional `context`). Specialized tools may explicitly state that
their own call stages one concrete approval; that is tool behavior, not policy.
Approve and deny both resume the requesting agent with the decision.

Policy-generated HITL is disabled. `[policy].mode`, `always_approve`, spend
thresholds and per-call judgement do not manufacture cards. `readonly` and the
emergency stop remain hard denials. An operator-authored workflow node with
`requires_approval = true` remains an explicit gate.

## Dormant policy-HITL taxonomy

This legacy classifier remains for audit, hard denials, and a possible future
policy-HITL mode. It does **not** create approval cards in the current product:

| Group | Effect kinds (examples) | Legacy `supervised` default |
| --- | --- | --- |
| **Spend** | `payment.send`, `subscription.start`, x402 outbound above cap | approval above `auto_approve_under_usd` |
| **Send** | `email.send`, `dm.external`, any first message to a new counterparty | approval for new counterparties; allowed for established threads |
| **Sign** | `filing.submit`, `contract.accept` | always approval |
| **Publish** | `external.publish`, Agent Card / price changes, website deploys | always approval |
| **Hire** | outbound A2A engagement with a new company; firing a vendor | approval above threshold or first-time counterparty |
| **Identity** | handle registration/renewal, key rotation, delegated signer mint/expand | always approval |

`readonly` still denies applicable effects, and the emergency stop still denies
ahead of every policy rule. `supervised`, `auto`, `full`, `always_approve`, and
the spend thresholds are otherwise classification and audit data only while
policy HITL is disabled. In particular, `auto` does not park outbound or spend
effects. Agents use `request_approval` when they need a decision.

The ordering and legacy evaluation tests remain so re-enabling policy HITL in a
future migration cannot silently change the old classifier. They describe that
dormant classifier, not current card creation.

Three of the four names are OpenHuman's own security tiers. `auto` is not, so
the mapping is no longer 1:1 and `PolicyMode::security_tier()` — the accessor
that asserted it was — has been deleted rather than made to lie. Where the two
vocabularies still have to meet, `harness::toolbelt::autonomy_for` borrows
`Supervised` for `auto`; the argument is on that function and matters, because
a workflow `tool_call` node has no `ApprovalPolicy` above it.

## Approval lifecycle

```text
agent calls request_approval ─▶ park (ApprovalId)
                                  │  surfaces in approvals inbox + chat
                                  ▼
                     operator resolves: approve │ deny
                                  │
                                  ▼
                     requesting agent continues
```

- **Default-deny on silence**: parked approvals expire to `deny` after a
  deadline — **24 hours** by default, set per company with
  `[policy].approval_ttl_hours`. Nothing irreversible ever happens because the
  Operator was on vacation.
- **Something has to run the sweep.** The deadline binds at resolution time
  regardless — `resolve_at` re-checks it under the same lock that removes the
  parked entry, so an overdue approval default-denies on the operator's click
  whether or not anything swept. Emptying the *queue* is a separate job, done
  by the process-wide `MaintenanceTicker` (`src/runtime/maintenance.rs`) once a
  minute for every registered company. Until issue #971 it rode the manifest
  cron scheduler, which is not spawned for a company with no `[[schedule]]` —
  so those companies parked approvals forever and swept none, at any age.
  Deadlines and the thing that enforces them are two features, and only one of
  them was shipped.
- **Retirement is a deny nobody made, and says so.** The journal records
  `ApprovalExpired { reason }`, the event log gets
  `ApprovalResolved { verdict: Deny, by: System }`, and the operator SSE frame
  carries `automatic: true` so the console can say "expired" rather than
  attributing the decline to whoever is looking. No grant is ever minted on
  this path: an approval that disappears must never read as one that was
  granted. Each card carries its own `expiresAtMillis`, so nothing vanishes
  unannounced.
- **Extend the deadline** (issue #1805). Showing the countdown is only half the
  answer to a run that would default-deny over a weekend; the other half is a
  lever to keep it alive. `POST
  /api/v1/companies/{id}/approvals/{aid}/extend` (and the single-company
  `/api/v1/company/approvals/{aid}/extend` alias) re-anchors a parked
  approval's TTL window to *now*, giving it a fresh full deadline, and answers
  with the new `expiresAtMillis` so the card redraws its countdown without a
  reload. It is guarded by the same company auth as resolve — keeping a stalled
  run alive is not an admin-only action — and 404s when nothing is parked under
  that id, so extending an approval that has since resolved or expired is told,
  not silently accepted. **A full fresh window, not "+N hours"**: the sweeper
  and the console both read `parked_at + ttl`, so moving that one instant is the
  whole of an extension and there is no second offset for a projection to
  disagree on. **It survives a redeploy**: the move is journaled as
  `ApprovalExtended` and replayed on boot (the gate is rehydrated from the moved
  anchor), so an extension is not quietly reverted the next time the process
  restarts. The payload timestamp (`at_millis`, the content's age) is left where
  it is — extending a deadline does not make the request fresher.
- **Edit applies only to concrete-action cards** staged by a specialized tool.
  A `request_approval` card is a question, carries no action to edit, and only
  returns its verdict to the requesting agent.
- Resolution requires operator auth ([runtime/api.md](../runtime/api.md));
  the resolving `Actor` is journaled.
- For `request_approval`, approve and deny both queue a verdict-bearing
  `ApprovalContinuation`; neither executes an effect or mints execution
  authority. Concrete-action cards explicitly staged by specialized tools keep
  their own execute-or-grant semantics.
- **Resolution is idempotent.** Resolving an approval that is no longer parked
  — a double-submit, a retried request, two operators on the same queue —
  is a no-op with a fixed reply. It writes no journal record and runs no
  follow-up cycle.

## Emergency stop (the governance kill switch)

`POST /api/v1/companies/{id}/emergency-pause` denies **every** new effect
outside the `Other` group, ahead of every policy rule including
`always_approve`. `POST .../emergency-resume` releases it. Both are
admin-scoped — a human must administer the company, not merely be a member —
and both take a confirmation phrase in the body. The hosting control plane's
machine principal is the one exception: it already owns or holds platform
scope over the company by the time it reaches these routes, so it is not
asked to be an admin on top of that. Normative:

- **It denies, it does not park.** Parking would make the approval queue an
  escape hatch from the switch: an operator could approve the very effects they
  just stopped without ever releasing it. Denial returns to the brain as a
  refusal it replans around, which is what "park all new work" has to mean.
- **`Other` stays allowed, so chat survives.** The operator has to be able to
  ask the company what it was doing. Effects classified as `Other` remain
  allowed while the emergency stop is engaged; the gate otherwise treats
  `Other` as a catch-all group, not a chat-only one, and does not police which
  tools it covers.
- **It is orthogonal to `lifecycle`.** `lifecycle = "paused"` rejects every
  request with a `409`, chat included — the opposite of what an emergency
  needs. A company can be `running` *and* stopped; resuming one does not resume
  the other. `GET /api/v1/companies/{id}` reports `emergency_paused`
  separately, and a console that reads only `lifecycle` will show a stopped
  company as healthy.
- **Already-parked approvals stay resolvable.** The switch gates `evaluate`,
  which runs before an effect executes; resolution does not pass through it.
  New work stops, in-flight decisions the operator was already asked for do
  not become unanswerable.
- **The event log is the durable state.** The last
  `CompanyEvent::EmergencyPauseChanged` decides, replayed at boot, so a stop
  survives a restart. There is deliberately no `CompanyRecord` field: a second
  copy of a safety flag is a second thing that can disagree with the first.
- **Fails safe.** If the log cannot be read at boot, the company comes up
  **stopped**. A company wrongly stopped is a visible, one-request problem; a
  company wrongly running is the failure the switch exists to prevent, and
  nothing would surface it.
- **Engaging is eager, releasing is durable-first.** Engaging flips the
  in-memory flag before journaling, so enforcement never waits on I/O that can
  fail. Releasing journals first and only clears the flag on success, so a
  failed write leaves the company stopped. The unsafe direction is never taken
  on a best-effort basis.
- **No timeout, ever.** Unlike a parked approval, the stop does not expire and
  is untouched by the TTL sweep. Only a deliberate `emergency-resume` by an
  identified operator clears it. A kill switch that lets itself go would resume
  work at 3am with nobody watching.
- **Both transitions are journaled with the acting `Actor`** and an optional
  operator note.

Confirmation is asymmetric on purpose: engaging takes the fixed phrase
`EMERGENCY-PAUSE` (an operator reaching for a panic button should not have to
look up an id), while releasing takes **the company's own id**, so the only way
out of the stop cannot be reached by replaying one body across companies.

Credential revocation — the other half of the audit's "emergency pause and
credential revocation" — is not covered here; it needs token scoping in the
harness.

### Settling the verdict is not running the follow-up

Resolving is two halves with very different durations, and the runtime keeps
them apart:

1. **Settle** — record the verdict and journal an `ApprovalContinuation` carrying
   the requesting agent and conversation. Milliseconds. When it returns, the
   operator's decision is permanent.
2. **Follow-up cycle** — a full agent turn that tells the requester approved or
   denied. It does not replay `request_approval` or automatically invoke the
   proposed action. Can take minutes.

The follow-up always runs on its **own task**, which the resolve then awaits.
That makes it drop-safe: a client that disappears mid-turn — a closed tab, or a
reverse proxy giving up on a slow upstream — abandons the *waiting*, not the
work. Fused, the two halves meant a dropped connection cancelled the
re-dispatch after the grant had already been spent, so the operator's approval
bought nothing and the conversation never resumed.

A resolve can also **detach** (`"detach": true`), answering the moment the
verdict is durable rather than holding the response open for the turn. The
continuation then arrives on the event stream's `agent_reply` frame. The
blocking form remains the default and its response body is unchanged.

A follow-up cycle that *fails* is logged host-side and leaves a recoverable
state, never a stranded one: the verdict and continuation are already durable,
and re-approving is the idempotent no-op above.

## Dormant concrete-action grant model

Policy-generated blocked-tool cards are disabled. [`grants.md`](grants.md)
documents the retained concrete-action machinery for in-flight records,
specialized tools that deliberately stage an action, and a possible future
policy-HITL mode. It is not the lifecycle of `request_approval`.

What lives there, in the order it appears:

- single-use grants: what approving a blocked **tool** call mints, versus a **native** effect the runtime performs itself (approving one of those executes it, per the lifecycle above);
- [standing grants](grants.md#standing-grants-this-tool-for-this-teammate-until-a-deadline) — this tool, for this teammate, until a deadline — and [what can never be granted broadly](grants.md#what-can-never-be-granted-broadly);
- [the `auto` tier](grants.md#the-auto-tier) that low-consequence middle defines, and [listing and revoking](grants.md#listing-and-revoking) live standing grants;
- [which tier a new company gets](grants.md#which-tier-a-new-company-gets) (issue #605);
- [what an `always_approve` entry names](grants.md#what-an-always_approve-entry-names-issue-684) (issue #684);
- [precedence at the tool gate](grants.md#precedence-at-the-tool-gate), and its step 7, [per-call judgement](grants.md#per-call-judgement-issue-338) (issue #338).

## Explicit approvals inside a workflow run

A workflow has two current explicit paths: an agent node can deliberately call
`request_approval`, and an author can mark a node `requires_approval = true`.
Policy classification adds no workflow gates.

**An agent node calls `request_approval`.** The node claims its run-scoped
approval queue and parks the question. Resolving it resumes that requesting
agent with the verdict; it does not replay a blocked tool or feed a fabricated
tool result into downstream nodes. The workflow run that produced the question
has already settled.

**A node marked `requires_approval`.** The engine reports these as node ids on
the run outcome, which reached the HTTP response and the `WorkflowRunFinished`
line — neither of which is an approval. Each pending gate now parks a
`workflow.approve` effect carrying the workflow id, the node id and the trigger
input, deduped on that triple so a re-run does not stack a second card for one
decision. It is a **native** effect (no `agent`), so approving performs it
rather than minting a grant.

### Approving a paused gate re-runs the workflow

The engine **settles** a paused run — nothing holds a task, a connection or a
continuation. So there is nothing to resume, and "continue" necessarily means
starting a fresh supervised run with the approved gate id unioned into the
trigger input's `approvals` array. The parked effect carries everything that
needs, which is what makes it survive a restart: journal replay rehydrates the
card and approving it still continues the work.

The cost is stated rather than hidden: **upstream nodes re-execute**. Agent
nodes re-spend tokens, and a reached `output` node **re-delivers** — a
warm-recipient email sends again, because the established-thread check is
state-based, not run-based. A gate normally sits *before* the side-effecting
node it guards, which is the entire reason to author one, so this is acceptable
for now; it is a real constraint on where a gate belongs in a graph.

A gate nobody decides ages out on the ordinary TTL to a default deny. Since the
paused run settled long ago, that costs nothing and cancels nothing.

### One run is continued once (issue #978)

The continuation unit is the **run**, not the node. A graph whose trigger fans
out to three gated nodes parks three cards, and before this each approval
independently re-dispatched the whole run: the spawn hung off `perform_effect`,
which fires once per approved effect, and each replay carried an `approvals`
array naming only its own node, so the other two paused and parked again.
Approving N gave N runs and N(N-1) new cards — 3 → 6 → 12 → 24. A staging tenant
accumulated 77 runs of one *disabled* workflow, 17 of which executed exactly one
node.

Three rules close it, and all three are needed — the first two on their own make
the console read correctly while the run table keeps growing.

1. **Every gate of one run shares a turn key**, `workflow-run:<run id>`, written
   by the park. Before, a workflow park recorded no key at all, so
   `approval_cycle` answered `Some(None)` and every branch believed it was the
   only decision outstanding: `stillAwaiting` read `0` on all three of three.
   It now counts down 2, 1, 0.
2. **The run is re-dispatched once**, when the last of its decisions lands,
   through the same `ContinuationQueue` an agent turn uses. A workflow run has no
   brain turn to continue, so the release re-runs the graph instead of running a
   cycle.
3. **The replay carries every approval the batch cleared**, so a sibling gate
   does not pause it and park itself again.

**Denials are final.** A refused node rides a third lineage ledger beside the
delivery (#438) and outward-call (#846) ones, under the reserved trigger-input
key `__opencompany_denied`. A listed node is not asked about again: the branch
below it simply never completes. Without it a mixed verdict would still net new
cards, and the invariant this issue exists for — **approving never increases the
number of pending approvals** — would be false. A TTL expiry is banked as a
default-deny on exactly these terms. A batch whose every gate was refused starts
no run at all; a mixed one runs, carrying the approvals it did get.

`stillAwaiting` stays **advisory**: it is a snapshot read on the request path
while the release runs detached. It is confirmation copy, not a control — the
continuation itself is decided under the queue's own lock, where no such race
exists.

**Two limits, stated rather than discovered.** A restart mid-round comes back
knowing only the gates still parked, so a batch released after it carries the
last decision and not the ones banked before it, and those un-carried siblings
re-park. That is inherited from #469 rather than added here — a workflow run is
simply the first thing to feel it. And a batch gets **one** spawn attempt: where
each approval used to have its own, a refusal at the concurrency ceiling (#401)
now loses the run with every card consumed, so it is announced on the operator
channel rather than only logged.

## Where the request is raised (issue #379)

An approval is not only a queue entry; it is an interruption of a conversation.
So a park records **which conversation** — `ApprovalParked.thread`, stamped by
the cycle from its own trigger events, surfaced on `ApprovalSummary.thread`, and
carried onto the explicit decision continuation when the approval resolves.

The id is `OperatorMessage.chat`: a desk id for a channel, a roster agent id for
a direct message. `Effect.agent` cannot stand in for it, and that is the whole
reason the field exists — a desk channel and a direct message to that desk's
lead are answered by the same teammate, so a request placed by asker would be
raised inside the wrong one of the two.

It follows the work rather than the queue entry. A resolution inherits the
thread of the approval it settles, so a follow-up turn that needs a **second**
sign-off re-parks in the channel the first was asked in instead of falling out
of the conversation. The verdict continuation is journaled into that thread too,
so the decision visibly returns to the place the operator was already reading.

The stamp is refused rather than guessed. A cycle batching two conversations,
or an addressed turn beside an unaddressed one, or beside a task dispatch,
stamps nothing. An approval with no thread — a workflow delivery, a scheduler
tick, anything parked before this shipped — belongs to no conversation and is
shown on the Approvals page alone, which is where every approval was shown
before. The page always lists everything; the in-conversation card is additive.

The event log carries the park itself (`CompanyEvent::ApprovalParked`) so the
card can appear live. It is deliberately thin — an id, a dotted kind, a thread —
because the effect's payload is redacted in exactly one place and must not
acquire a second. A reader re-reads the approvals feed for the rest.

### Dormant policy-card batching (issue #842)

This section describes retained legacy cards from a turn that policy gated in
several places. Current agents create one intentional question per
`request_approval` call, so this batching path does not manufacture or combine
their requests.

A research turn that reaches `espn.com`, `bbc.com` and `theguardian.com` parks
three approvals, and asking three times is the same fact told badly: it is one
piece of work, and every interruption costs a re-dispatch cycle that can
dead-end. So the parks a single turn raised are **surfaced as one request**.

The grouping key is not new. Issue #469 already journals the parking cycle, so
that a turn blocked on four decisions is continued exactly once when the last
one lands. `ApprovalSummary.batch` projects that same key, which is what makes
the two agree by construction: the batch an operator is asked about in one card
is precisely the batch the runtime holds a single continuation for. It is opaque
— an equality key, never an ordering, a count, or anything to show an operator.

**The grant model does not change at all.** There is no batch entity on the
host, no batch resolve on the wire, and nothing new in how a grant is minted,
stored or revoked. Each approval keeps its own id, its own verdict and — on
approve — its own host-scoped grant, so approving three fetches still leaves
three independently revocable rows under `Standing permissions`, one per host,
each with its own expiry. Batching the *asking* is not batching the *granting*,
and widening a grant to save a click would be exactly the leak `grants.md`
exists to prevent.

Two renderings over that one state, divided by what each surface is **for**:

- **Chat is the fast path: all-or-nothing.** One card per turn, listing the
  hosts it covers, with a single Approve/Decline and the ordinary scope
  control. Approve grants every call in the batch; Decline grants none. The
  operator is mid-conversation and wants one decision, not a form. It answers
  every item it is still asking about, because the turn stays blocked until each
  parked call has a verdict — a decision that left one open would hold the turn
  while looking as though it had resolved the card.
- **The Approvals page is the granular path: itemised.** One row per gated
  call, approved or declined on its own, matching how `Standing permissions`
  lists one revocable row per grant. It is where an operator goes for precision,
  or to clean up after the fact. A row says how many others came from the same
  turn, so someone arriving from the toast can tell one batch from an unrelated
  queue.

Granular control in *both* places would be redundant, and would double the state
that has to stay in step between two surfaces — so it lives in one.

**A decision that does not land is named, not swallowed.** One click fans out to
one resolve per item, so a failure on the third leaves two effects authorised
and one not. A toast is the wrong home for that — it does not say *which*, and
it is gone by the time the operator looks back at the card — so the row that
failed says so itself, the card counts the failures honestly (never "nothing was
recorded" about a click that authorised two of three), and the buttons stay live,
because a retry is the way out. A retry re-resolves only what is still pending.

The two must not drift, and do not, because neither owns any state: both render
the same feed, and both react to the `approval_resolved` frame. Deciding a row
on the page settles that item on the chat card without a reload, and the card
reports a partial state (`1 of 3 decided`) rather than going on claiming three
things are pending.

An approval with **no** batch — a workflow node, a scheduler tick, a park
journaled before #469 — is never grouped, not even with another one like it.
Absent means "the host did not say which turn this came from", and folding two
unknowns together would invent a batch out of a shared silence. Each is shown
alone, exactly as before this existed.

## Dormant delegation levels (standing rules)

These examples describe the retained policy compiler and a possible future
policy-HITL mode. They do not create approval prompts while policy HITL is
disabled.

Prosumers adjust the fence in plain language, which compiles to policy:

- "Auto-approve spending under $5" → `auto_approve_under_usd = 5.0`
- "Never contact my customers directly" → `never_do` → `Deny` on
  `dm.external` matching the customer list
- "You can post to the blog without asking" → remove `publish_artifact` from
  `always_approve` for that channel. Nothing to remove unless the operator put
  it there: `always_approve` defaults to empty, and under `supervised` it is
  the checkpoint taxonomy — not the list — that parks a publish

Standing-rule changes are themselves Charter edits with provenance and audit
([charter.md](charter.md)); loosening a rule takes effect for *future*
effects only.

## Audit

The approval log is immutable: every evaluate decision, park, resolution
(with actor and timestamp), expiry, and execution outcome is an `EventLog`
entry, and money-touching effects additionally journal to the ledger. The
operator surface renders this as plain history ("you approved sending the
Acme invoice on June 2").
