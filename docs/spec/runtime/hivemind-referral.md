# Cross-desk referral

Every mechanism in [`hivemind.md`](hivemind.md) stops at the edge of one desk.
A referral is the one that does not: a line committed on one desk may put a
question to another desk, and that desk answers with one real turn on its own
channel.

Off by default. A company that never writes the block deliberates exactly as it
did before this existed.

## Why a room needs one at all

Members of one desk read the same transcript, work the same part of the
company, and are wrong about the same things. Their errors are **correlated**,
and averaging correlated error does not remove it — so no amount of
deliberating inside a desk cancels a mistake every member of it shares. A desk
is a correlation boundary, and pooling *across* boundaries is the only
operation that can help.

tinyhivemind's own benchmark puts a number on it. Three desks, each confidently
wrong about a different option: deliberating inside the channels scored 0.2%,
and crossing between them scored 77.5%.

The same benchmark says when *not* to turn it on. At `--bias 0` — desks that
are individually unbiased — crossing a channel changed no answer and cost twice
the turns. The mechanism is for correlated error across desks; a company
without that should leave the block alone.

## Mention dispatch is this, with `reach = "local"`

`tinyhivemind` ships two seams that look like two features:
`mention_dispatch` / `MentionTurnQueue`, and `referral` / `ReferralQueue`. They
are not independent. With `enabled` and `max_hops` set and `reach` left at
`local`, a referral decision is **exactly** the decision `mention_dispatch`
makes, on the same conversation — an equivalence the library asserts with a
test over every interesting input rather than merely documenting.

So this host wires the wider seam once and reaches the narrower one through
`reach = "local"`, rather than carrying two queues, two policies and two
idempotency keys that have to agree. `@teammate` dispatch and `@#desk` referral
are one code path here and one manifest block.

## A question put to a desk is answered by the desk

`@#platform` resolves to exactly one agent in the library — that desk's first
eligible member other than the author — **before the decision leaves the
fold**, and there is no variant of `ReferralDecision` that carries two. That
resolution is positional, not a judgement about fit: on
`members = ["exchanges", "refunds"]`, `@#returns` selects `exchanges` every
time and `refunds` cannot be reached by a desk crossing at all.

Answering a question put to a *desk* with whichever seat is listed first on it
is the thing deliberation exists to stop, so this host convenes the far desk
instead: `HiveReferralRunner::deliberate` stands up that desk's own episode —
its `[hive]` policy, quorum, turn budget, move grammar and desk memory.
`direct_responder` is untouched, so a desk mention still cannot start a turn
through the ordinary responder ladder.

**What comes home depends on how the room ended.** A room that CONVERGED
answers with its closing report: that row names the proposal that carried and
who grounded it, which is the one sentence no single seat on that desk is
entitled to say. A room that did not converge has no such sentence, and its
report is bookkeeping — "Nobody on the desk had anything to add, so the room did
not open" — so relaying it would tell the asking room the desk had no view while
the view sat in the transcript. An unconverged room carries its members' own
turns instead, attributed and in order, each rewritten so the far desk's topic
and sequence markers do not travel into a desk where they name nothing.

Three things follow, and each is load-bearing:

- **The question is journaled on the desk being asked**, as the asking agent's
  own operator message, and the episode's thread is rooted on it. A single-seat
  crossing deliberately leaves nothing there; a room cannot work that way,
  because it folds its own transcript to decide the next turn. It is also what
  gives every turn a parent — without one, two crossings into the same desk in
  one cycle share the channel-level projection and fold each other's turns as
  votes.
- **The answer is credited to the desk**, not to the seat the library resolved:
  `room_note` writes "the Platform desk answered the question: …". A room's
  conclusion is not any one member's line, and seats may have argued the other
  way.
- **A crossing is one room deep.** The referred episode is given no federation.
  `max_hops` bounds a chain within one episode's ledger, and a referred episode
  has a fresh ledger at hop 0 — so a far desk allowed to refer onward could
  convene a third desk and nothing in the hop budget would see it.

A far desk that cannot hold a room — no `[hive]` block, one member, quorum out
of reach — falls back to the single-seat crossing, because each of those is an
ordinary shape a company is allowed to have. A room that was stood up and then
*broke* is a failed crossing, never retried as one seat: that would bill the
room's turns and then bill a turn again for the same question.

`deliberates = false` restores the single-seat crossing. It is the cheaper arm
and the one every measurement taken before this existed was taken against.

`max_hops` bounds how *deep* a chain goes. The library deliberately bounds
nothing about how *wide* one is, or what one costs, because only a host knows.
A crossing costs this host a full episode on another desk, so it adds
`peer_cap`: how many crossing questions **one episode** may ask, in total,
default 2. A question over the cap is refused, counted, and named in the closing
report.

## What crosses, and what does not

| Row | `chat_id` | `agent_id` | `text` |
| --- | --- | --- | --- |
| the far turn | the **far** desk | the teammate that answered | its whole answer |
| the answer, come home | the asking desk | `hive-referral` | `@<target> on the <desk> desk answered the question: …` |
| a question that got nothing | the asking desk | `hive-referral` | `@<target> on the <desk> desk did not answer the question.` |

The far turn is journaled on the far desk under the teammate that took it,
because that is what happened: a real turn by a real member on its own desk,
which its own desk should be able to read back.

The **answer** comes home under `hive-referral`, never under the answerer.
That is not a formatting choice. A row authored by a roster id folds as a trace
and can be counted as a supporter, so carrying the far desk's answer back under
its author's name would let one supporter count on two desks — which is voting
twice, not pooling information. The asking room hears another desk's reading and
still has to spend its own turns before anything is counted. Any leading `!` on
the answer is stripped for the same reason.

`hive-referral` is a second reserved author beside `hive-report` rather than a
reuse of it: both fold as system rows and neither can be spelled by a roster id
(every minter rejects a hyphen), but a reader that could not tell them apart
would be reading a peer desk's answer as this room's own summary. Only
`hive-referral` may appear more than once in an episode.

## Timing is load-bearing

The single largest effect the benchmark measured is not in the library at all.
**A desk whose members share a bias reaches quorum inside its own blind opening
round**, so an answer arriving after that is information the desk has already
voted past. The benchmark's first version had members ask *after* proposing —
which sounds more natural — and every desk committed to its own decoy with the
correction sitting three lines below the decision.

That is a host policy question, and this host answers it in two places:

- the peer block in the episode prompt says to ask **EARLY**, in as many words;
- the driver considers a line for referral *after* it is durable and *before*
  the next speaker is chosen, so an answer is on the board for the very next
  turn rather than one row late.

## The manifest knob

```toml
[[group_chat]]
id = "payments"
name = "Payments"
members = ["planner", "scout", "critic"]

[group_chat.hive.referral]
enabled = true    # off unless this says otherwise
max_hops = 2      # chain depth; 2 is one round trip
reach = "desks"   # "local" | "channels" | "desks"
returns = true    # carry the answer back to the desk that asked
peer_cap = 2      # crossing questions per episode (this host's own bound)
deliberates = true # a `@#desk` question convenes that desk, not one of its seats
```

Every field is optional; the block as a whole defaults to referring nothing.
Opting in and saying nothing else buys `max_hops = 2`, `reach = "desks"`,
`returns = true`, `peer_cap = 2`, `deliberates = true` — a desk that opted in
and got `local` would have opted in to nothing it could not already do, and one
that asked a peer *desk* for its judgement asked for more than one seat's
opinion.

`reach` widens strictly, which is why the library makes it one knob and not
two: a `@#desk` mention only means anything once a turn is allowed to run
somewhere other than here.

| `reach` | A target not on this desk | `@#desk` |
| --- | --- | --- |
| `local` | pulled into this conversation | ignored |
| `channels` | runs on their own desk | ignored |
| `desks` | runs on their own desk | convenes that desk (`deliberates`) |

### What validation refuses

Each of these would be **silently inert** rather than loudly wrong, and a desk
that asks nothing looks exactly like a desk whose members had nothing to ask —
so a typo has to be an error rather than a quiet no-op.

- an unknown `reach` word, named against the valid list;
- `max_hops = 0` or `peer_cap = 0` — a budget of nothing never asks anybody
  anything;
- `max_hops = 1` while `returns` is still on: a round trip is two hops, so every
  answer would be stranded on the desk that gave it. Write `max_hops = 2`, or
  `returns = false` if the question is meant to be one-way.

## What the host owes the library

The library names three obligations and will do none of them.

**Authorize both conversations.** A crossing referral writes into a channel its
author is not a member of. Here both come from the same `CompanyRecord` the
console reads — `desk_federation` builds the snapshot from the manifest's
declared desks and `effective_desk_members` — so a desk a member can name is a
desk the company agrees exists, and `@#id` can never resolve to a room nobody
is in.

**Carry the origin back.** When the host runs a referred turn it must hand the
`ReferralOrigin` to the *next* decision, or the answer has no way home. Nothing
in the library remembers it — that would be state, and the crate holds none. So
`referral::consider` folds twice: once on the committed line to get the
forward, and once on the far desk's reply, with the forward's origin supplied as
input, to get the `Return`.

**Bound the width.** `peer_cap`, above.

### Where this host is narrower than the port

`ReferralQueue` is documented as an atomic, durable enqueue boundary.
`EpisodeReferrals` is narrower, deliberately rather than unfinishedly:

- **Idempotency is per episode, not per journal.** The key the port asks to be
  scoped by — the `from` conversation plus the trigger sequence — lives in the
  adapter for the life of one `EpisodeDriver::run`. That is sufficient *and*
  honest, because an episode is not resumable: it runs to completion inside one
  process, and a crash leaves no half-run episode for a later process to retry.
  A durable idempotency record would be a table nothing ever reads a second
  time.
- **The child turn runs inline rather than being enqueued.** For the same
  reason — there is no turn queue to enqueue onto, and the episode loop is
  already the thing that runs turns one at a time — and for the timing reason
  above: an answer that lands after the room has voted is an answer that landed
  never.

A `Return` runs **no** turn at all. The far desk has already answered; a return
that ran a second turn would be the asker restating an answer it was handed,
which costs a model call to add nothing and lands a trace on the asking desk
under a member's own id.

## Failure

A referral never fails the episode.

- A far turn that does not finish is logged, counted, and left on the asking
  desk as `did not answer the question`. The room converges anyway — a question
  that went unanswered is a worse episode, not a broken one, exactly as one of
  the room's own seats missing a turn is.
- A malformed snapshot, a refused enqueue and a fold that declines are all
  "no referral": debug-logged, because the common reasons (`NoReferralTarget`,
  `SelfDesk`) are what every ordinary deliberation line produces every turn.
- A far desk whose answer cannot be journaled is reported and dropped rather
  than retried.

## The closing report

An operator reading the asking desk cannot otherwise see that the room went
outside, because the far turn happened somewhere else. So `EpisodeOutcome`
carries a `ReferralLedger` and the closing `hive-report` names it:

```
The desk settled on #answer42 after 6 turns (backed by theorist, programmer).
The room asked 1 question of another desk (@greeter on front).
```

with `N went unanswered` and `N more were declined by this desk's
`referral.peer_cap`` appended when either happened.

## Where the code is

| Path | Holds |
| --- | --- |
| `src/hivemind/referral.rs` | `ReferralConfig`, `HiveFederation`, `HiveReferralRunner`, `EpisodeReferrals` (the `ReferralQueue` impl), `consider` |
| `src/hivemind/types.rs` | `HiveConfig::referral`, `desk_federation`, `EpisodeOutcome::referrals` |
| `src/hivemind/episode.rs` | `EpisodeDriver::with_federation` and the per-line consideration |
| `src/hivemind/prompt.rs` | `EpisodePrompt::with_peers` and the peer block |
| `src/hivemind/mod.rs` | `HIVE_REFERRAL_AUTHOR` |
| `src/harness/built_in/brain.rs` | `HiveDeskRunner`'s `HiveReferralRunner` impl — the far turn on the far desk's channel |
| `src/company/manifest.rs` | what the block refuses |

## Testing

`src/hivemind/referral_test.rs` (19) scripts both seams — no model, no store —
because the properties are this host's, not a model's. Whether a room asks a
*good* question is the model's business; whether the answer lands on the right
desk, under an author that cannot be counted as a supporter, and only when the
desk opted in, is entirely ours. It covers: the snapshot (opted out, no peer,
peers named), the policy (defaults, each `reach`), the crossing (one turn on the
far desk by its first eligible member, the answer home and attributed, a far
`!support` that cannot fold here), the non-crossing (a line that asks nobody, a
desk that did not opt in), failure, `peer_cap`, idempotency, both prompt states,
and what validation refuses.

`tests/hivemind_e2e.rs::a_desk_asks_another_desk_and_only_the_information_crosses`
is the claim the unit tests cannot make: the far turn goes through the **real**
harness turn path on the far desk's channel, and a teammate on `front` informs
`lab` without ever being able to carry a topic on it.

## Not done

- **`MentionTurnQueue` is not implemented as its own port.** It is reached
  through `reach = "local"`, which the library asserts is the identical
  decision. A host that wanted the narrower seam for its own sake would add it;
  nothing here needs it.
- **A referred turn cannot itself refer.** `hop` is fed as `0` for a committed
  episode line and as the forward's `child_hop` for the answer, so a chain is
  one question and one answer. Deeper chains are what `max_hops` is for, and
  nothing here has wanted one.

See also [`hivemind.md`](hivemind.md) and
[`hivemind-deliberation.md`](hivemind-deliberation.md).
