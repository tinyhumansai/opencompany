# First-run company setup

*Three questions, asked once, that build somebody a working company before they
ever touch the console. Shipped in two phases: the team first, the workflows
once the team is proven.*

This document is written to be readable without knowing the codebase. Anyone on
the team should be able to read it, disagree with it, and say why. The last
section is the only technical part, and it exists so an engineer can pick this
up without guessing.

---

## This is not onboarding

Worth separating the two, because they get confused and they are not the same
job.

**Onboarding** teaches someone where the buttons are. A welcome card, a guided
tour, some tooltips. We already have that in the console.

**Company setup** is this document. It builds someone a business. When it is
done, there are named agents on the Company page — colleagues that did not exist
ninety seconds earlier. That page is the teammate card grid, which is what
`#/company` leads with since issue #1141; bare `#/team`, the address the roster
used to answer on, redirects there.

The tour is not the product. The company is. Setup runs first; the tour, if it
runs at all, comes after and now has something worth pointing at.

## The moment we are designing for

Today, someone signs in for the first time and gets a correct, working, empty
console. Every page is a page of nothing. The unspoken message is "here is your
toolkit, go build a company" — and building a company from a blank page is
exactly the work they came here to avoid.

We want the opposite message: *we already started for you.*

Someone answers three questions, watches their company get built, and lands on a
Company page with five agents on it. Their reaction should be "how did it know
that?" — followed immediately by wanting to fix the two things we got slightly
wrong. That second part is not a failure. Someone correcting our guess is
someone who has already accepted the premise.

## Two phases

**Phase 1 — build the team. Phase 2 — build the workflows.**

The split is not arbitrary caution. It follows a real difference between the two
things:

**An agent has no external dependencies.** We create it, it exists, it can be
talked to, and nothing outside our system has to cooperate. It cannot half-work.

**A workflow usually does.** "Check Meta ad spend daily" needs a connected Meta
account. "Dispatch orders" needs the store. On day one, none of those are
connected — so a workflow generated at setup time is, at best, waiting on a
connection nobody has made yet, and at worst visibly broken on the first screen
someone sees.

A broken workflow on day one costs more trust than a missing one. Phase 1
cannot fail that way, which is exactly why it goes first.

### What Phase 1 ships

**Built.** All of the following is implemented and covered by tests:

- All three questions (yes, including the automation one — see below).
- The build-out screen, showing agents created one at a time.
- A real roster on the Company page, each agent with a name, a role and a clear
  mandate.
- The answers stored, so Phase 2 never has to ask again.

One design change during the build, recorded because the earlier version is the
more obvious one: **the model designs the team rather than rewriting a curated
one.** The first cut had it polish a matched template's wording and swap at most
two roles, which meant someone who said "I sell homeware and run a YouTube
channel" got the e-commerce team with better sentences — the interesting half of
their answer could not reach the line-up. The curated rosters remain as the
phrasing reference in the prompt and as the fallback for every failure path, and
the host still enforces the shape (4–6 agents, no duplicate roles, mandate
length) after the fact rather than trusting the prompt for it.

### What Phase 1 deliberately does not ship

- No workflows.
- No connection prompts, no OAuth, no "connect your Meta account" step.
- No scheduled anything.

### Why we still ask about automation in Phase 1

This is the part that looks wrong and is not. Question 3 earns its place even
with no workflows being built, for two reasons:

1. **It makes the roster better.** Someone who says "Meta ads and order
   dispatch" is telling us to staff a Meta specialist and a logistics
   coordinator. Without that answer we are guessing from the industry alone.
2. **It becomes each agent's mandate.** The answer is what lets us write "owns
   campaign budgets and creative testing" under the Meta specialist instead of a
   generic job description. Specific mandates are most of what makes the roster
   feel authored rather than generated.

And it is stored. When Phase 2 lands, it builds workflows from answers the
person already gave, with no second interrogation.

### Is Phase 1 hollow without workflows?

Fair question, and the honest answer is: it depends on whether the agents can
do anything. They can — a person can open any agent and talk to it, brief it,
ask it for work. That is the existing chat surface and it works today.

So Phase 1 delivers a staffed company you can put to work by asking, rather
than an automated one that runs by itself. That is a real product, not a
placeholder. It is also the honest version of where the product is.

### When we move to Phase 2

Not on a date — on evidence. The bar:

- **Do the agents survive?** If most of a generated roster is still there a week
  later, our guesses are good. If people delete it, they are not — and
  generating workflows on top of a roster people reject would multiply the
  error, not fix it.
- **Does anyone talk to them?** An untouched roster is decoration.
- **Does setup complete?** If people drop out at question 2, the workflow
  question is not the problem.

If the roster is landing, Phase 2 turns the stored answers into workflows inside
the same setup flow. If it is not, we fix the roster first. Phase 2 is worth
nothing on top of a bad Phase 1.

## The three questions

| # | What we ask | What it decides |
|---|---|---|
| 1 | What kind of company are you setting up? | The domain — everything downstream keys off this |
| 2 | What team do you need? | The roster of agents we create |
| 3 | What are you trying to automate? | Phase 1: each agent's mandate. Phase 2: the workflows |

Three questions, one screen each, under a minute total. Every question changes
what gets built. If a question does not change what gets built, it does not earn
a screen — the test to apply to anything anyone wants to add here.

**Question 2 should be pre-filled, not blank.** Once someone has said
"e-commerce", we already have a good idea of the roster. Show it, already
ticked, and let them add or remove. The difference between *authoring* a team
and *adjusting* one is the difference between work and delight, and it costs us
nothing.

**Question 3 should take messy input.** People will type "social media posts,
Meta ads, report generation, order dispatch" as one run-on list. That is the
correct thing for them to type and we should handle it. A neat picker with
twelve checkboxes would collect less.

## The build-out is the product

This is the part to get right. Everything else is a form.

When someone hits the final button, they should watch their company being
assembled. Not a progress bar. Not "Setting up…". **Named things appearing, one
after another.** Phase 1:

```
Creating your team…
  ✓ Meta Ads Specialist — campaigns, budgets, creative testing
  ✓ SEO Specialist — product listings, organic traffic
  ✓ Logistics Coordinator — dispatch, tracking, returns
  ✓ Fulfillment Manager — suppliers, stock levels, and what the shop needs to keep selling
  ✓ Accountant — reconciliation, margins, spend

Setting up their desks…
  ✓ Done — your team is ready.
```

Phase 2 adds a second block underneath for workflows. The screen is designed to
grow that way, so Phase 2 is an addition rather than a rewrite.

Two notes that matter more than they look:

**Show each agent's mandate as it appears**, not just the name. The one-line
mandate is the proof that we listened to question 3 — it is what turns a list of
job titles into a team that looks assembled for this specific business.

**Do not make it instant.** This is the one screen in the product where faster
is worse. If the work finishes in two seconds, pace the reveal anyway. Someone
watching their company get built is enjoying it; someone who blinked and missed
it just filled in a form.

## Worked example: the e-commerce business

Someone answers:

1. "E-commerce — I sell homeware online."
2. *(accepts the suggested team, removes one, adds "customer support")*
3. "Social media posts, Meta ads, generating my reports, order dispatch."

**Phase 1 — the team we create:**

| Agent | Mandate (drawn from question 3) |
|---|---|
| Meta Ads Specialist | Campaigns, budgets, creative testing |
| SEO Specialist | Product listings, organic traffic |
| Logistics Coordinator | Dispatch, tracking, returns |
| Fulfillment Manager | Suppliers, stock levels, and what the shop needs to keep selling |
| Accountant | Reconciliation, margins, spend |
| Customer Support | *(added by the user at question 2)* |

Note how directly question 3 shapes that middle column. "Order dispatch" is why
the logistics coordinator has a mandate rather than a job title.

**Phase 2 — the workflows we would then build:**

| Workflow | Runs | Does |
|---|---|---|
| Daily sales report | Every morning | Pulls yesterday's numbers, writes the summary |
| Meta ad spend check | Daily | Flags overspend or a campaign falling over |
| Order dispatch | On new order | Moves the order through fulfilment |
| Weekly content schedule | Mondays | Drafts the week's posts, holds them for review |

Every one of those needs a connection that does not exist on day one.

## The question we deliberately do not ask

An earlier draft had a fourth question: *how much should your agents do on their
own?* It is cut, and the reason matters.

Someone ninety seconds into the product cannot answer that. They have not seen
an agent do anything yet. Asking it puts a governance decision in front of
someone with no basis for making it, and breaks the spell at the moment we are
trying to cast it.

**Decision D1: we set it ourselves, to supervised, and we do not ask.**
Supervised means agents do the work but anything with real consequence —
sending, publishing, spending, anything reaching outside the company — stops and
waits for a person. It is already the system's default and already the right
answer on day one.

We surface it *after*, once someone has watched an agent draft something and has
an opinion. It matters more in Phase 2, when things start running on a schedule.

## Everything we build is a suggestion

**Decision D2: the console says so, in words.** Every agent can be renamed,
retired or replaced. The framing is "here is a starting point", never "your
company is ready" — the first invites correction, the second sets us up to be
judged on whether we guessed perfectly, which we will not have.

## The model step comes first

**Decision D8: the credential is settled before the questions, and a failure is
visible at the moment it happens.**

It sat third, after the three questions, on the reasoning that cheap interesting
questions earn the right to ask for a credential. That reasoning was right about
motivation and wrong about consequence: the design pass falls back to a curated
team on *any* failure, so a missing or mistyped key produces a plausible company
rather than an error, and the operator finds out two screens later if at all. The
one step whose failure invalidates every answer after it belongs before them.

`POST /api/v1/setup/inference/test` is a live one-turn probe of what the operator
typed, built through the same `resolve_endpoint` the runtime uses — a test passing
under different rules than the runtime applies is worse than no test. It persists
nothing: finding out a key is wrong must not require having stored it. Provider
errors are summarised to one actionable line rather than forwarded, since a
failure body can echo request material into a browser on an unauthenticated host.

Local endpoints get one onboarding-only convenience before that shared
resolution: a bare `localhost:port` gains `http://` and `/v1`. Setup reads the
OpenAI-compatible `/models` catalog and probes its concrete model rather than
sending the abstract `agentic-v1` tier to a server that cannot resolve it. The
successful provider, endpoint and tier mapping are persisted only when Finish
creates the company; the probe itself remains write-free.

The step is a **gate with an escape**. `GET /api/v1/setup` reports whether the
host already holds a credential, so a hosted tenant — whose operator has no key
and no way to get one — arrives with the step already answered and only needs to
press Test. Where nothing is configured, an explicit "continue without a model"
proceeds to the curated team. Decision D3 is not negotiable: a credential the
operator cannot obtain must never be the one thing that traps them.

A secret's *presence* is reported, never its bytes — so there is nothing to
pre-fill the key box with, and the configured state is drawn as a settled fact
rather than an empty field. An empty box with placeholder text reads as an
unanswered question — on a hosted tenant, the one impression it must not give.

## What a teammate can reach on day one

Every designed teammate starts with the workspace and nothing outward: the focus
belts name `workspace`, `docs`, `files` and sometimes `web`, and never `composio`
or `media` (decision D7). So a Social Media Manager cannot post and a Cold Email
Specialist cannot send, until the operator connects an account.

That is the right posture — reaching a real account is an act only a person can
authorise, and at first run there is nothing connected — but a roster reads as a
set of capabilities, and saying nothing repeats the failure the twelve invented
teammates were deleted for: offering what the host cannot honour. The review
screen states it. The fix is the sentence, not a wider grant.

An `outreach` focus carrying `composio` was rejected: it would put credential
reach behind a value a model chose from free text — what D7's enum prevents.

## What is on the board on day one

A company that boots with a correct roster, correct ledgers and an empty To-do
column has agents with nothing to pick up and an operator with no idea where to
start, so the first thing anybody does is invent the setup list — badly, and
differently each time. So the board is seeded once, at first boot, from `globals/tasks.toml` (the setup
every company has: the brief, the first goals, the standing decisions, the top
risks, the connections) plus that bundle's own `companies/<name>/tasks.toml`
(the setup its vertical is defined by, winning on a shared id). Each card names
the ledger row or document it should produce, so an agent that picks one up
hands back the thing rather than an essay about it.

Every seeded card lands in **To-do**, unassigned, and none of them can dispatch:
a seed file cannot name a column, and the seeder writes through the plain task
store rather than the edge-firing path that turns `in_progress` into a run. A
freshly provisioned company can boot with a full board and spend nothing.
Seeding is first-boot only, so a card an operator deletes stays deleted. See
[globals.md](../globals.md) for the rules and the `disable = ["task:<id>"]` opt-out.

## What the host enforces, rather than asks for

Five guarantees hold whatever a model returns — coverage checked against the
host's own job list, a tool belt asked for rather than inherited, standing
instructions the host writes rather than the model, a copy of the reference team
refused the name "designed", and a fallback that says which fallback it is. Each
is a boundary rather than a line in a prompt, and each has a test that fails when
it stops holding:
[company-setup-guarantees.md](../company-setup-guarantees.md).

## Nobody gets stuck

**Decision D3: if the step that dreams up the roster fails, we fall back to a
sensible generic team and let them in anyway.** The model being down, a timeout,
a bad response — none of these should strand somebody on a setup screen. Getting
in with a roster that is only roughly right is dramatically better than not
getting in. A person who cannot get past setup has had the worst possible first
five minutes and is not coming back.

**Decision D3a: say it before the questions, not after the roster.** A host with
no model still answers — that is D3 — but the operator has by then spent three
answers believing they were shaping something, so the dialog states the
consequence beside question one. Three things keep that honest. It asks the
**company** (`GET {scope}/inference`), whose cognition path decides whether a
roster builder exists; `/api/v1/setup` reads the *host's* credential and refuses
multi-company hosts, so a BYOK company reads as unavailable there while its
design pass runs fine. The wait is **bounded**, because nothing dismisses this
dialog and the questions are withheld while the check runs. And leaving to wire
a model is **not a skip but a debt**: it records a resume and reopens setup on
the return — the controller outlives the navigation and bars a second unprompted
open, so merely not persisting the skip would still strand them.

**Decision D3b: the fallback says which fallback, because the next action
differs.** `no_model` means nothing was reachable — wire a credential.
`model_unreachable` means a credential is wired but the provider did not answer
— check the connection or retry. `not_designable` means a model answered
unusably, almost always because the answers were too sparse — the retry
restarts the questions in place so the operator can say more about the business
(the Company page's own setup prompt is unreachable once the fallback team
staffs the company). "Add a model in Settings" shows only for `no_model`;
elsewhere it would send someone to fix a credential that had just worked.

**Decision D3c: redesign replaces the fallback team rather than stacking on it.**
When an operator follows "Add a model in Settings" after receiving a fallback,
the controller records a redesign debt that names the fallback team's rows. On
return, setup reopens over the staffed company, removes only **those** rows, and
creates the newly designed roster. The global baseline is preserved, and so are
teammates other operators staffed while model settings were open — the debt
names what the first pass created, rather than re-reading the roster on return
and treating everyone else's work as part of the team being replaced. The
completion screen's in-place "Try again" records the same debt (the rows the
failed pass just created) before restarting the questions, so a reload or crash
before that replacement lands can reopen setup in redesign mode rather than
leaving the gate reporting staffed with no way back in. The debt is settled the
moment the replacement lands, not when the operator clicks a completion action:
a designed replacement clears it (the owed redesign is done, exactly as
finishing setup would), a replacement that fell back again re-keys it to the new
fallback's rows, and a rollback that could not remove every partial row extends
it to name the survivors — so a reload on the completion screen, or before a
retry after a refused rollback, never reopens against a boundary the landing
just deleted or misses a row it left behind.

## How we know it is someone's first time

The obvious approach is to tick a box in the browser. The tour does this today,
and for a tour it is fine — worst case someone sees a welcome card twice.

For setup it is not fine, because setup *creates things*. Someone who clears
their browser data or signs in from their phone would run it again and get a
second team stacked on the first.

**Decision D4: no flag. We ask a question we already know the answer to — has
anybody staffed this company yet?** Nobody staffed means setup has not run.
People on the team means it has. This survives a cleared browser, a new laptop,
and a second person joining the same company, with no stored flag to drift out
of step with reality.

**"Staffed" is narrower than "has a roster", and the difference is load-bearing.**
The [global baseline](../globals.md) merges a fixed set of teammates into *every*
company whatever its manifest says, and they cannot be deleted — `DELETE
…/team/{id}` answers `409` on each. So "is the roster empty?" is false on every
company this product can serve. Asked that way, as it was until issue #1404, the
gate never opened anywhere: not on a fresh tenant, not on `companies/e2e_setup`,
which exists for no other purpose than to reach it.

The question the console asks is therefore *has this company any teammate that
is not part of the baseline every company gets*. It reads that from the roster
itself — `GET …/team` carries `global` on each row, the same `Agent::global`
marker the merge sets — rather than from a list of the baseline's ids copied
into the console, which would break silently the next time a global is added.

It also happens to be exactly the signal Phase 2 needs, so nothing here has to
change later.

The gap: someone who completes setup and then deletes every agent gets offered
setup again. Acceptable, and probably correct.

### What defining first-run this way actually costs

Only obvious once it ran. **Every company under `companies/` declares a roster**,
so none of them can ever reach the flow — there is no way to demo or test setup
against the shipped examples. `companies/e2e_setup` exists for exactly that: a
company that ships with nobody on it, which the end-to-end lane runs against.

And the lane has to actually run it. It did not, for as long as the flow was
broken: `frontend/test/e2e/company-setup.spec.ts` carried a `test.skip` written
for a host serving the wrong company, no CI job set the variable that would have
selected the right one, and once the baseline landed the guard fired on every
run. The console lane was green over a feature that could not open. The spec now
**asserts** its host instead of skipping, `playwright.config.ts` selects it only
in a first-run run (`npm run e2e:first-run`, which serves `companies/e2e_setup`
on a data root of its own), and `Console E2E (first run)` runs it through
`scripts/ci/assert-e2e-spec-ran.sh` — which fails on a reported count of zero,
because a lane whose only guarantee is its configuration can be silently
vacuous.

Two related problems surfaced in the same run, both fixed:

- The Team page fabricated a twelve-agent starter roster whenever the host
  answered with nobody, so "this company has no team yet" rendered directly above
  twelve agents that did not exist on the host; a genuinely empty company now
  shows an honest empty state. Once the gate reopened, the same sentence had to
  go for the mirror-image reason: the prompt renders above the baseline's
  teammates, who *do* exist, so it now reads "this company hasn't been set up
  yet" — true whether the roster below it holds nobody or the baseline's four.
- The product tour is held not only while the dialog is open but for as long as
  the company is unstaffed. Otherwise skipping setup popped the tour's welcome
  straight over an empty console — the first impression this document exists to
  replace. The hold is a render-time gate, not a state one: the tour's own effect
  consumes a one-shot resume marker and must not be made to re-run.
- Skipping is not a one-way door: the empty-team prompt remains, and anyone can
  reopen setup from **Settings → Product tour → Set up company** or `#/setup`,
  which forces the dialog open even for a staffed company — an explicit request,
  not the automatic first-run offer.

## Open questions

### Phase 1

1. **Can you skip it?** Kind to people who just want a look around, but an easy
   route back to the empty console. Recommendation: allow it, keep the offer
   visible afterwards rather than burying it.
2. **How many agents?** Four to six feels right. Too few looks thin; too many is
   clutter someone now has to tidy.
3. **How many company types do we hand-tune?** A curated roster for e-commerce,
   content, agency and consulting will beat a generated one every time. The long
   tail has to be generated. Where is the line? Curating the top few is also the
   fastest route to a good demo.
4. **Does the Company page say what to do next?** Phase 1 hands someone five
   colleagues and no automation. If the page does not suggest "open one and give
   it a brief", the roster risks being admired and then ignored.

### Phase 2

5. **Do generated workflows start switched on?** Live-by-default is a stronger
   impression and a worse surprise if one misfires. Leaning: on, but every one
   holding at a review step so nothing reaches the outside world unseen.
6. **What happens to a workflow whose connection is missing?** This is the whole
   reason for the phase split and it needs a real answer before Phase 2 —
   probably created but visibly dormant, with one obvious button to connect the
   thing it is waiting on.

---

## For engineers

Everything above is intent; where it meets code — the tables that map each
phase's needs to routes, and the notes on the decisions — moved to
[engineers.md](engineers.md), the part most likely to go stale.
