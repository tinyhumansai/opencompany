# Grants and the tool gate

What happens when the OpenHuman tool policy blocks a call and the Operator
approves it: single-use and standing grants, the `auto` tier they define, the
tier a new company is given, what an `always_approve` entry names, and the order
the gate decides in. This doc is normative.

Policy-generated HITL is currently disabled. The machinery below remains for
approvals already in flight, specialized tools that explicitly stage an
approval, and a future opt-in policy mode. New general approvals come from
`request_approval`; `supervised`, `auto`, and `full` do not create cards.

Split out of [`approvals.md`](approvals.md), which was over the repository's
500-line limit. That page holds the trust model around this one — the checkpoint
taxonomy, the [approval lifecycle](approvals.md#approval-lifecycle) the "as
above" below refers to, the emergency stop, and the *effect* gate. This page is
the *tool* gate.

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

The same standing-policy record also carries a **verdict**. Approving a card
with a standing scope creates a standing approval; declining it with that scope
creates a standing **denial**. A denial is an agent-only refusal for the same
`(subject, tool, scope)` the card showed: matching calls are rejected until the
policy expires or is revoked, rather than being parked again. Workflow gate
cards cannot mint standing denials, because the workflow path does not enforce
that verdict; the resolve route rejects that combination with a 400 and leaves
the card parked for a one-time denial instead.

Standing approvals and denials are mutually exclusive for a scope. When a new
standing policy is minted, any live opposite-polarity policy whose subject and
tool match and whose recorded scope overlaps the new scope is journaled as
revoked before it is removed from the live set. Thus the newest operator
decision wins, while policies for different hosts or other non-overlapping
scopes coexist. Overlap is symmetric: a wildcard policy (scope `None`) shadows
every policy for the same tool in either direction. A wildcard legacy policy
is superseded by any newer scoped decision, and a wildcard **new** policy
supersedes every older scoped one — the newest decision is the whole standing
contract for that tool, rather than leaving the older scoped policy
listed-but-inert until the wildcard expires and resurrects it.

Expiry is enforced at **redemption**, under the same lock as the match, and also
swept on the scheduler's maintenance tick. The sweep is housekeeping and an
operator notice; it is never the enforcement, or "for one hour" would mean
"until the next tick after one hour".

#### What can never be granted broadly

Decided by what a tool can **reach**, not by what it is called.

This used to be "exactly the tools whose consequence group is the catch-all
`Other`", on the reasoning that delegating to the taxonomy beat keeping a second
hand-written set of safe tools. The reasoning was right and the measurement was
wrong. `Other` is where a tool lands when the classifier finds no consequence
word *in its name*, so the three broadest capabilities in the system —
running an arbitrary command, reaching an arbitrary address, and overwriting the
guidance the operator wrote — were all grantable, while a repository read scoped
to one connected account was not. The bucket also conferred membership by
omission: adding a tool and not thinking about it handed it the longest
permission available.

So the two questions are now separate answers from one declaration
(`src/policy/consequence.rs`), one entry per tool:

- **may it run unattended?** — `readonly` denies anything that mutates or
  reaches outside; `supervised` parks it; `auto` parks only the part of it that
  leaves the company. This is what the approval card is for.
- **may an operator hand it over for a stretch of time?** — refused for anything
  that can execute arbitrary code (`shell`), reach an arbitrary address
  (`http_request`, `curl`, `web_fetch`, `git_operations`), act through a
  third-party server (`mcp_call_tool`), change the company's shared note tree
  (`workspace_write`, `workspace_create`, `workspace_rename`,
  `workspace_delete`), or run a saved workflow; refused for
  every named
  consequence — Spend, Send, Sign, Publish, Hire, Identity; and refused for any
  tool nobody has declared.

What stays grantable is the low-consequence middle the feature exists for:
writes confined to the agent's own sandboxed workspace (`file_write`, `edit`,
`apply_patch`, `csv_export`, `memory_store`), and **Composio reads**.

### The `auto` tier

That low-consequence middle is the base of the `auto` tier (issue #560), with
one company-context exception: a shared-workspace mutation confined to a node
the calling agent created and last wrote runs unattended too (issue #877). An
operator had two settings and needed a third: `supervised` parks every write
including the agent's own scratch files, so companies drown in cards; `full`
parks nothing but the always-ask list, so it stops asking before a shell
command or a git push too. Companies ran one and suffered, or ran the other and
lost the gate that mattered.

`auto`'s contract, in the operator's words: **the agent works without
interrupting me, and stops before anything that leaves the building or spends
money.** The line is one predicate,
`Consequence::parks_under_auto` — a call parks under `auto` when it would park
under `supervised` **and** it is not `Standing::Grantable`.

It reads the existing declaration rather than adding a list, because the split
was already there and argued tool by tool. That reuse does widen what
`Grantable` means — from "an operator may hand this to one teammate until a
deadline" to "this runs unattended for everyone while the company sits in
`auto`" — which is sound because `Standing` is decided by what a tool can reach
rather than by what it is called, and because the widening is exactly what the
operator chooses when they select the tier. It is written down so that a later
edit loosening `Grantable` knows it is loosening two things.

The workspace exception is graded at the policy seam rather than in the
declaration table, because it is not a property of the tool: `workspace_create`,
`workspace_write`, `workspace_delete` and `workspace_rename` all stay
`PerCall` in the table, and the harness policy asks the live company tree
whether the resolved node was both created and last written by the calling
agent. Only then does it lift that one call to `Grantable`, so the agent's own
notes run unattended under `auto` while an operator- or teammate-authored node
still parks. The lift never reaches the standing-grant mint path, which keeps
reading the table — so the shared note tree stays non-grantable (above) however
`auto` treats a call on it. Two extra bounds follow from what the tools
themselves allow: `workspace_delete` is refused at execution time when a
folder still holds anything (the policy allows an owned target, so no approval
card is parked), and `workspace_rename` of a folder parks unless every node
inside it is the agent's own, because the rename re-renders all of their paths. A `workspace_rename` that *moves* a node is
bound by the same landing-zone rule `workspace_create` applies to a nested
parent: the destination folder must also be the agent's own (the home root
excepted), so an operator-authored folder inside the home cannot become an
unreviewed collection point.

Three tools answer the reach question from their **arguments** rather than from
their name, because the name is too coarse to be the answer. `composio_execute`
reads the action slug (issue #441), `web_fetch` reads the URL's host (issue #673),
and `shell` reads the command itself (issue #875).

`shell` is the one an operator feels. Classifying the name meant `grep -c foo
*.log` and `rm -rf /` were the same input, so an agent that investigates by
grepping its own workspace bought an approval card per command — under `auto`,
whose contract says it stops only before what leaves the company, and a grep
does not. The grading is the vendored runtime's own
`SecurityPolicy::classify_command`, the classifier OpenHuman gates its shell
tool with: it splits the command into unquoted segments, classifies each against
a curated safe-read allowlist, takes the **maximum** — so `grep x && rm -rf /`
is destructive, not a read — and lifts a redirect or a `tee` to a write.
Anything it does not recognise is a write. Only a provable read is downgraded to
`Reach::Nothing`; every other class keeps `Reach::Consequence` and
`Standing::PerCall`, so it parks exactly as before and can hold no standing
grant. A self-declared `category` argument may raise the class and never lower
it, and a call whose command cannot be read stays gated.

Two boundaries `auto` deliberately does not draw:

- **`Reach::Money` does not park.** `web_search` is billed but changes nothing,
  and it already runs unattended under `supervised` for a reason that binds
  harder here — openhuman resolves a `RequireApproval` inline and never
  re-dispatches, so a parked search is a search that never happens and an agent
  with no search invents citations. `auto` must not be stricter than the tier it
  replaces. The per-agent daily cap holds spend, and it sits above the tier
  dispatch. Generation that spends on *submit* (`media_generate_image`,
  `media_generate_video`) is a `Consequence` and parks.
- **`always_approve` is not consulted by the tier at all.** It is checked above
  the dispatch and wins over every tier, `full` included.

The tier composes with the Composio read/send reclassification rather than
depending on it: a Composio read is `Grantable` today while still carrying
`Reach::Consequence`, and reclassifying its *reach* leaves its standing alone,
so it runs unattended under `auto` either way.

#### And on the effect gate (issue #1454)

Everything above is the **tool** gate. The **effect** gate —
`ManifestApprovalGate`, which decides the native effects the runtime performs
itself — is a second dispatch on the same `[policy].mode` word, and #560 gave it
no `auto` arm. `auto` fell into its fail-safe catch-all, so the tier every
provisioned company boots on parked *every* native effect, behaving exactly like
`readonly` and strictly stricter than the `supervised` tier below it on the
ladder. Nothing caught it because a tier with no arm and a tier that decided to
park return the same decision, and every gate test named a single mode.

It has a named arm now, and that arm applies the
[supervised checkpoint taxonomy](approvals.md#checkpoint-taxonomy) unchanged.
That is not a placeholder. The split `auto` needs on the tool path is
`Standing::Grantable` — the sandbox writes that stay inside the company — and
the effect taxonomy has no equivalent: every group it parks leaves the company
or spends money, which is the line `auto` advertises stopping at, and its one
inside-the-company group (**Other**) is already allowed under `supervised`. So
there is nothing left for `auto` to loosen there.

The stricter reading — park `Spend`/`Send`/`Hire` unconditionally, withholding
`supervised`'s cap relief and established-thread relief — was considered and
rejected: it inverts the ladder a second time, parking a $1 spend and a reply on
a running thread that the tier *below* waves through.

Two properties are pinned by tests rather than by this prose: every word in
`POLICY_MODES` reaches a named arm, and permissiveness is monotonic up the
ladder for every branch of the taxonomy.

`auto` is **not** the default. Issue #560 argues it should become one, but
`default_policy_mode()` returning it would change behaviour for every existing
company with no `[policy]` block on its next load, so that is its own decision.

`composio_execute` is the reason arguments are part of the question. Every
Composio action arrives under that one tool name, so classifying the name
collapsed "list a repository's pull requests" and "send an email" into one
verdict, and the cautious answer had to win for both. The action slug in the
arguments is classified instead, against the provider's own curated catalogue —
a read is grantable, a send is not, and **anything the catalogue does not name
is a send**. That last part is not a detail: an action nobody has classified
might do anything.

Because a standing grant carries no arguments, the policy re-classifies the live
call before honouring one, so a scope granted on a GitHub read cannot admit an
outgoing email on the same tool name.

The console hides the control for a card it cannot be used on; that is UX. The
**enforcement** is host-side at mint time, so a hand-rolled request for a
standing grant on a Send-group tool is a 400 rather than a permission. Native
effects are refused too: the runtime performs those itself, so "this tool, for
this teammate" names neither of the two things it needs.

A refused scope changes nothing at all — the approval stays parked and no
verdict is journaled — so the operator can simply approve it once instead.

**Not yet as narrow as it should be.** A standing grant records a tool, not a
toolkit, so "this teammate may read from GitHub for eight hours" is expressed as
"may make Composio reads for eight hours". Reads only, never sends — but broader
than the sentence an operator actually consented to. Narrowing it needs a scope
field on the grant record.

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
Defensible because a standing grant only ever admits tools declared grantable
and never a priced call; a per-use record is additive later.

### Which tier a new company gets

Issue #605. A **new** company is given `auto`; an **existing** one is never
re-tiered. Two knobs, and that distinction is the decision:

- `default_policy_mode()` — what a manifest with no stated `mode` *parses* to.
  Stays **`supervised`**.
- `PROVISIONED_POLICY_MODE` — what provisioning *writes into* a new company's
  manifest when its author stated none. **`auto`**.

Every shipped preset declares `mode` outright, so both paths that read one
(`serve --company <dir>`, the desktop app) arrive carrying a tier.
`POST /api/v1/companies` is the only creation path with no template behind it,
and is where the write happens.

**Why not simply move the parse default.** That value answers for every manifest
ever parsed, including the re-parse of a company that has run for months, so
moving it re-tiers existing companies rather than creating them differently.
Two things then go wrong:

1. Every company with no `[policy]` block silently widens on its next load.
   Nobody edited anything and nothing is written down.
2. Less obvious and worse: the persisted record stores the *defaulted* value, so
   a silent `company.toml` that parsed to `supervised` last boot parses to `auto`
   this boot — and `carry_policy_override`, which is `previous_seed ==
   next_seed` over the whole block, reads that as version control speaking and
   **discards the operator's console `[policy]` override**, including one that
   had *tightened* the tier.

Writing the tier at provisioning avoids both, and makes it legible: a
provisioned tenant has no `company.toml`, so the record is the only place its
tier appears. `a_provisioned_company_with_no_stated_tier_is_recorded_on_auto`
and `a_provisioned_company_keeps_whatever_tier_it_states` pin both halves, the
second walking `POLICY_MODES` so a fifth tier cannot escape the guarantee.

### What an `always_approve` entry names (issue #684)

One namespace, not two. An entry is an **effect kind**, and a **tool name is an
effect kind** — the harness projects a flagged tool call onto an `Effect` by
making the tool name the kind verbatim, so `publish_artifact` is the
single-segment case of the same thing `payment.send` is the two-segment case of.
Both approval paths — the native-effect gate and the harness tool policy — read
one matcher, `policy::always_approve::matches`, which matches the exact entry or
a leading dotted segment (`payment` gates `payment.send`, never
`payroll.export`). They previously held two matchers with two rules, so one
operator list meant different things depending on which brain was running.

Operator-authored entries are intentionally not restricted to the declared tool
table. Native effect kinds are open by specification: the Medulla wire carries
`kind` as a free string, and a hosted brain may emit one this repository has
never seen. The gate checks `always_approve` before its `EffectGroup` fallback,
so even an otherwise-unclassified custom kind can be exactly the one the
operator meant to stop. Treating the classifier as a registry would turn a
working fence into a load error.

The **default is empty** because shipped defaults can and should meet a stricter
standard than open operator input. The old default was `payment.send` /
`filing.submit` / `external.publish`; none named a tool, so every company using
the harness path believed payments and publishing were gated and none were. Two
of the three name capabilities the product does not have; the real name behind
the third is `publish_artifact`, which must not be defaulted because `full`
publishing unattended is the ruling on issue #658. Under the default
`supervised` mode the checkpoint taxonomy parks every `Spend` / `Sign` /
`Publish` effect anyway, so the empty default costs no protection that was ever
real. A drift test requires every future default entry to name its intended,
declared tool target explicitly.

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
   standing grant can never admit money by placement rather than by promise —
   and it re-checks the live arguments, so a grant minted on a Composio read
   cannot admit a Composio send.
4. `[policy].always_approve` → park.
5. the per-agent daily cap, then `auto_approve_under_usd`.
6. mode dispatch (`readonly` / `supervised` / `auto` / `full`).
7. **per-call judgement** (issue #338) → park. Consulted *only* where step 6
   allowed the call, and its only answers are "stop" and "say nothing".

The grant sits **above** `always_approve` on purpose. A tool on that list still
parks the first time, which is what the list is for; but once the operator has
approved that specific call, re-parking it would mean approval authorises
nothing at all for precisely the tools an operator most wants to gate
deliberately. Single-use, exact-args and agent-scope are what keep that
narrow.

Note this is the *tool* gate (`ApprovalPolicy`), which is a different path from
the *effect* gate (`ManifestApprovalGate::evaluate`) the taxonomy above
describes. A harness tool call parks directly and never reaches `evaluate`.

### Per-call judgement (issue #338)

Steps 1–6 are all decided before the run starts, by an operator writing a
manifest; nothing in them looks at what the run is about to do. Step 7 closes
that gap: `src/policy/judgement.rs` asks, per candidate call, whether it
warrants a human on its own merits, and it can **only ever add a stop**.

Step 7 is also the one step scoped by **which path the call arrived on** (issue
#674): #338's rules govern an agent turn, and #614's position governs a node an
operator authored into a saved workflow — unless that node's arguments are
templated from an upstream node's output, which returns it to the agent rule.
Steps 1–6 decide identically on both paths, so `always_approve` still gates a
workflow node.

It is documented in full — the placement argument, the three rules, the path
split and its boundary condition, the acceptance verbs per path, the no-learning
boundary against #563, why it is not a model call, fail-closed, and the
`publish_artifact` carve-out #658 ruled correct — in
[per-call-judgement.md](per-call-judgement.md).
