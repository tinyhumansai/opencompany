# What a turn is allowed to spend

The two ceilings that bound one teammate's turn — the tool-iteration cap and
the in-turn spend brake — what each one stops, and why a run that hits one must
never be reported as having hit the other.

Split out of [`agents.md`](agents.md) on this repo's 500-line cap for a Markdown
file. That page owns how a teammate is declared and what reaches its prompt;
this one owns what a turn built from it may spend.

Two ceilings bound one turn, and they bound different things.

## Tool iterations — 25

A turn may run **25** tool-calling rounds before the runtime pauses it and hands
the operator a resumable checkpoint instead of an answer.

The number is stated by this crate, not inherited. It used to be inherited: the
agent builder was never told a cap, so every teammate silently ran on the
vendored default of ten. Ten is a summariser's budget. A product manager asked
for a feature spec reads the standards, reads the release checklist, reads the
nearest prior spec, drafts, and publishes — and spends the ten before delivering
anything, which is the incident the raise comes from.

Twenty-five is ~2.5x headroom over that shape without the 5x of the runtime's
"extended" 50. **Cost grows faster than the multiplier**: each iteration re-sends
a transcript longer than the last one's, so 2.5x the rounds is more than 2.5x the
spend. That is why the ceiling is the smallest number that covers the observed
work rather than the largest one that would be safe.

It is a **global** default, deliberately: it reaches every shipped template
without editing each one. A teammate cannot raise or lower it from its manifest
today.

**Reaching it is a pause, not a failure.** The runtime stops the tool loop, asks
the model once more (with tools withheld) for a resumable "Done so far / Next
steps" checkpoint, and returns that as an ordinary successful reply. There is no
error to catch and no error to match on, which is precisely why a capped turn
used to be invisible — the operator read a tidy plan with no deliverable behind
it and no way to tell the agent had been cut off mid-task. So the harness reads
the runtime's cap flag while the turn's agent lock is still held and carries it
out on the turn's outcome, OR'd across every seat turn of the round behind one
operator message. When any of them paused, the operator gets a **second,
unauthored bubble** after the reply saying the turn stopped at its step limit,
that nothing errored, and that replying "continue" asks the agent to pick up
from there. It is a separate bubble rather than an addition to the reply because
the reply — and only the reply — is written back to the context store as memory;
appending would file the platform's notice as something the agent said and
recall it into later turns. See `src/harness/built_in/mod.rs`
(`TurnOutcome::hit_iteration_cap`), `src/hive/round.rs` for the fold across a
round, and `src/harness/built_in/brain.rs` for the notice.

## In-turn spend — armed only for a teammate with a declared daily budget

The company's other two spend controls — the plan-level token ceiling and a
teammate's `budget_usd_daily` — are both **pre-dispatch**. They decide whether a
turn may *start*; neither can see inside one. So a turn that begins one cent
under a cap can finish over it, and raising the iteration ceiling widens that
window in proportion. How far over depends on whether anything is watching from
inside: for the plan-level token ceiling, and for a teammate who declares no
daily budget, nothing is, and the overshoot is unbounded. A teammate who does
declare one gets the in-turn brake below, which bounds it to a single call.

A running turn is therefore additionally metered by an in-turn brake — openhuman's
`BudgetStopHook` — an after-call threshold check installed between iterations.
It records each completed model call, then compares cumulative spend
(`TurnCost::total_usd()`) against the cap and pauses the turn before the next
provider call once spend is at or beyond it. The brake is installed **only** for
a teammate who declares a `budget_usd_daily` cap. Because the check runs after a
call has already been charged, a crossing call lands on the ledger before the
next one is prevented — the turn can finish at or slightly above the cap, so the
worst-case overshoot is bounded by a single model call rather than an entire
turn ("one call" rather than "one turn, of unknown size").

This mirrors the vendored runtime's own posture rather than inventing one.
OpenHuman constructs `BudgetStopHook` nowhere — it is an available primitive, not
an applied policy — and the only hook it installs is `GoalBudgetStopHook`, opt-in
and tied to a user-declared goal. Its own docs are explicit: *"we never
hard-stop a user-present turn that isn't actively burning a live budget."* So a
teammate with no declared budget gets no in-turn brake, and there is deliberately
no blanket per-turn dollar figure that no operator can see or change (it would
not be in `company.toml` and not in the console). Four shipped templates do set
`budget_usd_daily` — three agents in `signals_opportunity_studio` and one in
`e2e_harness` — so the opt-in path is genuinely exercised, not dead code.

Since a budget halt and an iteration-cap pause are different outcomes, the
runtime reports them separately: `TurnOutcome::hit_iteration_cap` is read off
the turn's progress stream (`progress_pump::hit_iteration_cap`), which stays
`false` for a hook-driven stop — the run paused below the 25-round ceiling, so
the cap predicate never held. A cap pause means the teammate ran out of rounds
with work still to do and can be resumed via the "continue" bubble above; a
budget halt means it ran out of money, returns whatever reply the model produced
before the hook fired, and gets no such bubble today. Anything that renders one
to an operator must not label it with the other.
