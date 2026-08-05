# Checkpoints and Approvals

The trust core of the product: agents act freely inside the fence; anything
irreversible waits for the Operator. This doc is normative.

## Checkpoint taxonomy

Effect kinds that MAY require sign-off, grouped by what they risk:

| Group | Effect kinds (examples) | Default in `supervised` mode |
| --- | --- | --- |
| **Spend** | `payment.send`, `subscription.start`, x402 outbound above cap | approval above `auto_approve_under_usd` |
| **Send** | `email.send`, `dm.external`, any first message to a new counterparty | approval for new counterparties; allowed for established threads |
| **Sign** | `filing.submit`, `contract.accept` | always approval |
| **Publish** | `external.publish`, Agent Card / price changes, website deploys | always approval |
| **Hire** | outbound A2A engagement with a new company; firing a vendor | approval above threshold or first-time counterparty |
| **Identity** | handle registration/renewal, key rotation, delegated signer mint/expand | always approval |

`readonly` mode gates *every* effect; `full` mode auto-allows everything
except `[policy].always_approve` entries. Modes mirror OpenHuman's security
tiers so an OpenHuman-backed `ApprovalGate` maps 1:1.

## Approval lifecycle

```text
effect emitted ─▶ evaluate ─▶ Allow ─▶ execute, journal
                      │
                      ├─▶ Deny ─▶ returned to brain as refusal (it replans)
                      │
                      └─▶ RequireApproval ─▶ park (ApprovalId)
                                              │  surfaces in approvals inbox + chat
                                              ▼
                            operator resolves: approve │ deny │ edit
                                              │
                                              ▼
                            ApprovalResolved event ─▶ follow-up cycle
```

- **Default-deny on silence**: parked approvals expire (default 7 days,
  configurable) to `deny`. Nothing irreversible ever happens because the
  Operator was on vacation.
- **Edit** lets the Operator amend the effect payload (fix the email, lower
  the amount) and approve the amended version; the brain sees both the
  original and the edit.
- Resolution requires operator auth ([runtime/api.md](../runtime/api.md));
  the resolving `Actor` is journaled.
- Approve executes the parked effect exactly once
  (journal-before-execute, [runtime/lifecycle.md](../runtime/lifecycle.md));
  deny feeds the refusal back so the brain replans rather than retries.
- **Resolution is idempotent.** Resolving an approval that is no longer parked
  — a double-submit, a retried request, two operators on the same queue —
  is a no-op with a fixed reply. It writes no journal record and runs no
  follow-up cycle.

### Settling the verdict is not running the follow-up

Resolving is two halves with very different durations, and the runtime keeps
them apart:

1. **Settle** — record the verdict, journal it, mint the grant (or execute the
   native effect). Milliseconds. When it returns, the operator's decision is
   permanent.
2. **Follow-up cycle** — a full agent turn, so the brain learns the verdict and
   re-issues the granted call. Can take minutes.

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
state, never a stranded one: the verdict and grant are already durable, and
re-approving is the idempotent no-op above, so a retry mints no second grant.

## Approving a blocked tool call: single-use grants

Two different things park on this queue and they need opposite treatment.

A **native** effect is one the runtime performs itself (an email, a workflow
delivery). Approving it executes it, as above.

A **tool call** is one an agent tried to make and the OpenHuman tool policy
blocked. There is nothing for the runtime to execute — the parked effect's
payload is the tool's *arguments*, and only that agent can run the tool. So
approving it mints a **single-use grant** and re-dispatches the agent to
re-issue the call. Without this, approving recorded a verdict and nothing ran:
the operator had to go back and ask for the same thing again.

A grant is:

- **single-use** — redeeming consumes it, so one approval buys one call and
  never standing permission;
- **agent-scoped** — a grant minted for one desk does not admit another's
  identical call;
- **argument-exact** — matching is whole-value equality on the arguments. A
  re-issue with a different recipient or amount does not match and re-parks,
  because the operator never saw those arguments. Approve-with-edit mints
  against the *amended* arguments;
- **time-boxed** — 15 minutes. An approval is consent to act now, not a
  standing authorisation. An unredeemed grant expires and the operator is told
  the agent did not act, so re-approving is an informed choice.

The lifecycle is journaled (`ApprovalGranted` → `GrantConsumed` | `GrantExpired`)
and replayed on boot, so a restart between approving and re-issuing does not
drop the approval. Consumed and expired grants are folded out on replay: a
resurrected single-use grant would not be single-use.

### Standing grants: this tool, for this teammate, until a deadline

Single-use is the right default and was, for a while, the only scope — which
made it the whole design. An agent reaching for the same tool repeatedly
produced a stream of near-identical cards, and the operator's rational escapes
were approving blind or switching the company to `full`, throwing the gate away
to stop it nagging. So an approve now carries a **scope**:

- **just this once** — the default, and byte-identical to the single-use grant
  above. Needs no interaction: a body with no `scope` key is exactly the
  pre-#374 request.
- **this tool, for this teammate** — a **standing grant**, with a mandatory
  duration of 1 hour, 8 hours, or 7 days.

A standing grant is a distinct type from a single-use one, not a scope flag on
it, and both differences are load-bearing: it has **no arguments field**, so it
cannot argument-match or be widened into doing so; and its **expiry is not
optional**, so it cannot live forever. The duration is stored as an absolute
epoch-millis deadline, capped at 7 days server-side — a request past the cap is
a **400, never a silent clamp**, because quietly shortening a duration would
leave the operator believing a permission is live when it lapsed days earlier.

Expiry is enforced at **redemption**, under the same lock as the match, and also
swept on the scheduler's maintenance tick. The sweep is housekeeping and an
operator notice; it is never the enforcement, or "for one hour" would mean
"until the next tick after one hour".

#### What can never be granted broadly

Exactly the tools whose consequence group is the catch-all `Other`. Anything the
classifier calls **Spend, Send, Sign, Publish, Hire or Identity** stays a
per-call decision, forever, with nobody having to remember to add it to a list —
the rule delegates to the taxonomy at the top of this document rather than
keeping a second hand-written set of "safe" tools, which would be written once
and then silently accrue every new tool by omission.

The console hides the control for a card it cannot be used on; that is UX. The
**enforcement** is host-side at mint time, so a hand-rolled request for a
standing grant on a Send-group tool is a 400 rather than a permission. Native
effects are refused too: the runtime performs those itself, so "this tool, for
this teammate" names neither of the two things it needs.

A refused scope changes nothing at all — the approval stays parked and no
verdict is journaled — so the operator can simply approve it once instead.

Known and deliberate: `shell` classifies as `Other` and is therefore grantable.
Narrowing it belongs in the classifier — giving shell a consequence class — not
in a second ad-hoc exclusion list, which is the drift this design exists to
avoid. It is operator opt-in, time-boxed, revocable, and both `never_do` and the
`readonly` brake outrank it.

#### Listing and revoking

`GET {scope}/grants` lists the live standing permissions; `DELETE
{scope}/grants/{gid}` takes one back, 404 when it is already gone. Both are
journaled with the **resolving operator's real identity**, so "who opened this
up" is answerable later. Revocation takes effect on the tool's **next** call — a
call already admitted is not aborted, because there is no abort lever inside an
agent's turn and killing one mid-call is the lifecycle anti-pattern; the next
policy check finds nothing and re-parks.

The list carries no arguments, because a standing grant has none — so it opens
no second redaction surface.

Lifecycle records: `StandingGrantMinted` → `StandingGrantRevoked` |
`StandingGrantExpired`. Replay folds out revoked and expired grants *and*
anything already past its deadline, so a host that was down across an expiry
cannot hand the permission back on boot.

Per-use auditing is tracing-only in v1: mint, revoke and expiry are journaled
with actor and timestamps, but each admitted call writes no journal record.
Defensible because a standing grant only ever admits `Other`-group tools and
never a priced call; a per-use record is additive later.

### Precedence at the tool gate

A tool call is decided in this order:

1. `never_do` hard-deny — **reserved**; the delegation-rule compiler is still a
   Phase-1 stub, so no tool-level arm exists yet. It sits above the grant
   deliberately: a grant is an operator saying yes to one call, `never_do` is
   the company saying not ever, and the standing rule is the one meant to
   survive a socially engineered operator.
2. a live **single-use grant** matching agent + tool + exact arguments →
   allow, once.
3. a live **standing grant** matching agent + tool, unexpired → allow. Placed
   immediately below single-use consumption so a matching single-use grant
   still *burns*: masking it would leave the operator's one-off approval to
   expire and be announced as "the agent didn't act", about a call that ran.
   This arm refuses a **priced** call (a declared amount, a metered read), so a
   standing grant can never admit money by placement rather than by promise.
4. `[policy].always_approve` → park.
5. mode dispatch (`readonly` / `supervised` / `full`).

The grant sits **above** `always_approve` on purpose. A tool on that list still
parks the first time, which is what the list is for; but once the operator has
approved that specific call, re-parking it would mean approval authorises
nothing at all for precisely the tools an operator most wants to gate
deliberately. Single-use, exact-args and agent-scope are what keep that
narrow.

Note this is the *tool* gate (`ApprovalPolicy`), which is a different path from
the *effect* gate (`ManifestApprovalGate::evaluate`) the taxonomy above
describes. A harness tool call parks directly and never reaches `evaluate`.

## Where the request is raised (issue #379)

An approval is not only a queue entry; it is an interruption of a conversation.
So a park records **which conversation** — `ApprovalParked.thread`, stamped by
the cycle from its own trigger events, surfaced on `ApprovalSummary.thread`, and
carried onto `GrantedCall.origin_thread` when the approval mints a grant.

The id is `OperatorMessage.chat`: a desk id for a channel, a roster agent id for
a direct message. `Effect.agent` cannot stand in for it, and that is the whole
reason the field exists — a desk channel and a direct message to that desk's
lead are answered by the same teammate, so a request placed by asker would be
raised inside the wrong one of the two.

It follows the work rather than the queue entry. A resolution inherits the
thread of the approval it settles, so a follow-up turn that needs a **second**
sign-off re-parks in the channel the first was asked in instead of falling out
of the conversation. And the redeemed grant's continuation is journaled into
that thread too — approving something visibly causes the next thing to happen,
in the place the operator was already reading.

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

## Delegation levels (standing rules)

Prosumers adjust the fence in plain language, which compiles to policy:

- "Auto-approve spending under $5" → `auto_approve_under_usd = 5.0`
- "Never contact my customers directly" → `never_do` → `Deny` on
  `dm.external` matching the customer list
- "You can post to the blog without asking" → remove `external.publish`
  from `always_approve` for that channel

Standing-rule changes are themselves Charter edits with provenance and audit
([charter.md](charter.md)); loosening a rule takes effect for *future*
effects only.

## Audit

The approval log is immutable: every evaluate decision, park, resolution
(with actor and timestamp), expiry, and execution outcome is an `EventLog`
entry, and money-touching effects additionally journal to the ledger. The
operator surface renders this as plain history ("you approved sending the
Acme invoice on June 2").
