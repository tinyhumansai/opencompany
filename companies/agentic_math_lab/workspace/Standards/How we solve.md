# How we solve

The rules below are the ones that have actually caught something. Each is
stated with what it caught, because a rule with no incident behind it is a
preference and gets argued with.

## An answer is two routes agreeing

One program's output is a result. It becomes an answer when a second route,
written from the problem statement rather than from the first program, reaches
the same number. Anything short of that is on [[Attempts]], not on [[Answers]].

## Cost the naive method before being clever

State what brute force would cost as a count of operations. Half of these
problems are settled by a loop; the other half are ones where "about 10^14
operations" is what makes the search for structure obviously necessary. Both
answers are useful and neither takes long to work out.

## Small cases are the oracle

The most obviously-correct method you can write, run on the smallest inputs,
is what turns "the program ran" into "the program is right". A fast program
that disagrees at n=5 is a fast wrong program.

## Read the statement literally

Inclusive or exclusive bounds, distinct or not necessarily distinct, proper
divisors or all of them, below or up to. Most wrong answers here were the right
answer to a nearby question. Restate the problem in your own words before
solving it, and name the reading you took where it was ambiguous.

## Exact arithmetic unless the answer is a decimal

Integers stay integers. A float that agrees to twelve digits is wrong when
fifteen were asked for, and it is wrong silently.

## Keep the failures

A wrong program is labelled and kept, and an abandoned approach is recorded
with what killed it. The alternative is exploring the same dead end twice,
which is the most common way a run here wastes an hour.

## The network is not an instrument

This company holds no web or search tools, on purpose. The answer to a stated
problem is something the lab computes; a number that arrived any other way is
not evidence that anything here works.

## An approval request is a pause, not a prompt to improvise

Policy does not stop a program automatically. Before any `shell` or
`run_workflow` action, the agent calls `request_approval` with the exact program
and reason, then stops until the operator decides. The program call must not
appear in the same turn. The wrong response to waiting is to produce the number
some other way. The first run of this lab did that: refused its sandbox, it did
the arithmetic in its head, reported the right answer, and the end-to-end test
went green over a lab that had computed nothing. An answer with no program
behind it is a recollection, and this lab does not ship recollections.
