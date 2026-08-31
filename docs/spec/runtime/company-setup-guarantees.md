# First-run setup: what the host enforces

The product decisions behind first-run setup live in
[company-setup.md](company-setup/overview.md). This file holds the five things the host
*enforces* rather than requests, and why each is a boundary instead of a line in
a prompt.

A prompt is advice. Every rule here was a prompt instruction first, and each one
was observed to fail: coverage went unchecked while the prompt asked for it, the
reference team came back verbatim while the prompt called it a quality bar, and
an unrecognised tool focus quietly widened a teammate's authority instead of
narrowing it. What follows is the enforcement that replaced the hoping.

## The claim is checked, not trusted

**Decision D6: the host splits the jobs, the model claims which it owns, and the
host checks the claim.** "Every job they mention should have an obvious owner"
was in the prompt from the start, and nothing verified it — a prompt is advice.

So `job_items` (in `src/company/setup.rs`) splits the automation answer on the
separators a person actually types, numbers the items, and sends them as a
checklist. Each returned agent lists the numbers it owns in `covers`. After
validation the host computes `uncovered` by set maths over **its own** list.

The order matters: if the model both listed the jobs and reported covering them
it would be marking its own homework, and the two halves would agree by
construction. Coverage is only a check because something other than the answer
decides what was asked for.

A gap buys exactly **one** re-ask, which marks the unowned items in place and
keeps the first ask's numbering — renumbering the gaps from zero makes the second
answer's `covers` refer to a different list, and the two silently disagree. One,
because a second is a conversation and somebody is watching a build-out screen:
if naming the missing jobs outright did not produce an owner, a third phrasing
will not either. What survives is reported to the operator on the review screen
rather than hidden — an honest gap they can act on beats a roster that quietly
ignored a third of what they asked for.

A roster that is **entirely** the reference team is reported as curated, not
designed, whatever produced it. The reference roster goes into the prompt as a
quality bar, and a model that reads it as a menu can hand the whole thing back —
an answer whose shape is perfectly valid, so validation admits it, and the
operator is then told "built from what you told us" about a roster nobody
designed. The guard is on the line-up, not the prose: one role of the model's own
is a decision, and a team that borrows a sentence is still designed. The prompt
asks for the operator's own words; the host only refuses to call a copy an
original.

Coverage is a claim only the design pass makes. A curated fallback was chosen by
keyword and never read the list, so it reports its provenance instead and claims
nothing.

## A teammate asks for its tools

**Decision D7: a designed agent names the belt it needs, and the model picks a
job shape rather than a tool.**

`manifest_from_setup` builds from a name-only manifest, so `[tools]` took the
globals `default_allow` — and an agent whose `tools` list is
empty inherits the company belt whole. Every teammate a first-run operator
created therefore held shell, code, web, subagent, files, docs, **media** (real
money) and **composio** (per-tenant credentials), for a company described in
three sentences. The globals teammates sitting next to them already do the
opposite, and `globals/agents/researcher.toml` says why: a request is intersected
with `[tools].allow`, so naming one can only ever narrow.

Each proposed agent now carries an `AgentFocus`, named for what the teammate
*produces* — `research`, `writing`, `design`, `analysis`, `build`, `operations`,
`coordination`, `support` — and the host maps it to a belt. The model never
names a tool. Tool grants are a permission boundary, and letting free text a
stranger typed reach `[tools]` would put that boundary inside the prompt's blast
radius; a closed enum means the worst a hostile answer achieves is the wrong belt
from a list the host wrote. No focus may name the catch-all `*`, and a test
quantifies over the whole vocabulary so a focus added later cannot turn a belt
back into an inheritance.

Every shape starts from one base belt — `workspace.read`, `docs.*`, `files.*`,
`web.*`, `search`, `mcp:*` — and adds what its own work needs: workspace writes
for the shapes that produce rather than report, `media` for `design`,
`composio` for `operations` and `support`, `subagent` for the two that move
work between people, and `shell` + `code` for `build` alone.

The belts used to stop at the workspace, documents and files, and that is the
thing this decision changed. A teammate a first-run operator created could not
search the web, could not call an MCP server the operator had installed, and —
because `workspace.*` confers *reads*, not writes — could not write the
workspace it was told it owned. Each of those reached the operator as the
teammate reporting its own tools as "not enabled", and as a Team screen listing
the ask under "asked for but not granted". Withholding by default only works
when somebody is standing there to grant it; on a company minted from three
sentences in a wizard, nobody is. The narrowing that remains is the one that
can be acted on: an agent's `tools` line is intersected with `[tools].allow`,
so a company drops a namespace from that one list and every teammate loses it
at once.

`repo` is the exception, and for a reason that is not a preference: a host on
filesystem storage refuses to boot a company whose allow-list names it, so it
is absent from the default grant and from every belt. A MongoDB-backed company
that wants it adds `repo.*` to both.

An unrecognised focus gets the narrowest working belt, never an empty list. It
used to inherit, on the reasoning that an unknown value should degrade to the
pre-focus behaviour — and that inverted the control, because an empty `tools` list
means *inherit the company belt*. An **invalid** focus therefore produced a wider
agent than any valid one, and the operator's free text reaches a model that writes
that string. Fail closed: `writing`'s belt is the floor, which is the base belt
plus workspace writes — so a tampered focus still reaches no shell, no code, no
repository, no media budget and no Composio credential.

The curated templates declare a focus too. An operator with no credential must
not end up with the *wider* company.

## A teammate is told how to work, and the host writes it

**Decision D9: the focus that picks a belt also picks a set of standing
instructions, and the model authors none of them.**

`manifest_from_setup` set `id`, `role`, `description` and `tools` and left
`prompt` unset. So everything a setup-built teammate was ever told was what
`persona_prompt` assembles — "You are Fulfillment, the Fulfillment Manager at Acme. Speak
in the first person as this role." plus a mandate capped at 200 characters.
Around 150 characters of instruction, sitting on the same roster as a globals
teammate carrying 500–600 (`globals/agents/*.toml`). The mandate says what a
teammate owns; nothing said how it works.

`AgentFocus::instructions` supplies that, keyed on the same closed enum that
already picks the belt, for the same reason D7 gives: an agent's prompt is the
single field that decides how it behaves, so letting the pass author it would put
a stranger's free text — read by a model, written into a system prompt —
inside the prompt's blast radius. The model names a work shape; the host owns
every word of the standing instructions.

Not every word the teammate is told, which is worth stating exactly because the
looser claim is easy to write and false: `persona_prompt` already appends the
**mandate**, and the model wrote that. Model text reaches the system prompt
today. What it reaches under is a 200-character cap, a field whose only job is
to name what the teammate owns, and a review screen the operator reads before
anything is created — a much smaller surface than a free-form instruction block,
and the reason a per-teammate instruction field is a separate decision rather
than an obvious extension of this one.

The instructions describe the *shape* of the work and never the business. The
mandate already carries the business, and a second copy in the same prompt would
be a staler one. They are written in the globals' register without reusing its
sentences: a global teammate is on the same roster, and two agents given the
same instructions are one agent twice.

### The shape is a floor, and the profile sits on it

A shape cannot be the whole answer, because a shape is shared. `analysis` covers
seven of the thirty curated profiles, so an SEO Specialist and an Accountant were
told the same thing however carefully that text was written — the collision
above, one level down.

So each curated profile carries its own line too, appended after the shape's:
what *this role* is judged on, rather than how its kind of work is done. "A
stock-out costs more than reordering slightly early" is not something the
`coordination` shape can say. Every template now reads five distinct instruction
sets for five teammates, where the widened vocabulary alone got `software` to
four and `ecommerce` to three.

Shape first, profile second, because the profile qualifies the general case —
the order the persona already reads in, where role and mandate arrive before
anything about how to work.

**The profile text is looked up, never carried.** The obvious implementation
hangs it on `ProposedAgent` beside `focus` and lets it ride the review-screen
round trip. That would be a hole. `focus` survives the trip safely *because* it
is a value from a closed enum the host re-parses — the worst a crafted request
achieves is the wrong belt from a list the host wrote. Free-form instruction text
posted back would reach a teammate's system prompt verbatim, authored by whoever
made the call, and the company-scoped setup route is deliberately open to any
member rather than only the operator. So `manifest_from_setup` re-matches the
template from the same answers and reads the text out of its own compiled-in
tables. A request that invents an `instructions` field is ignored, and a test
posts one to prove it.

An operator who **renames** a role on the review screen drops its profile line
and keeps its shape. That is the answer rather than a gap: once "Report Writer"
is "Reports", the host no longer knows the teammate is that profile, and
inheriting a mandate from a role somebody deliberately changed is worse than
falling back.

An unrecognised focus is instructed with **nothing**, which is the opposite of
what the belt does with the same input — and deliberately. A belt substitutes
because a permission has a safe direction to fail in; instructions have none.
Telling an analyst "never invent a detail to make a sentence work" is worse
guidance than the role framing it already has, so an unknown shape keeps exactly
the pre-instruction behaviour.

The curated templates declare a focus, so the fallback team is instructed too —
an operator with no credential must not end up with the *less directed* company.

## A fallback says which fallback

Three different situations produce the curated team, and the review screen said
"we couldn't reach a model to tailor it" for all of them. That is false in the two
where a model answered and its answer was unusable, and it matters because the
operator's next move differs: *add a key* versus *tell us more about the
business*. `FallbackReason` carries `no_model` or `not_designable` to the console,
which says the right sentence.
