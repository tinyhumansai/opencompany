# Drafting a mandate or a persona

The two read-only routes behind the teammate copilot (issue #1776), split out of
[`api-write-plane.md`](api-write-plane.md) to keep that file under the
repository's 500-line ceiling. Everything here is part of the console write
plane; neither route writes.

`POST …/team/{agentId}/draft` runs one turn of a conversation about one of two
fields — `description` (the mandate on the roster card) or `instructions` (the
persona appended to the teammate's system prompt). `POST …/team/draft` does the
same for a teammate the operator is still filling in on the Add form, which has
no id yet; it takes the `role` being typed (blank is a `400`) and the other
authored fields alongside.

The body carries `messages`: the conversation so far, oldest first, each
`{role: "operator" | "copilot", text}`. Empty means the opening turn — "draft
something, I have not said anything yet" — which is deliberate: an operator
staring at a blank persona box wants a starting point to react to, and making
them type first asks for the thing they opened the copilot because they could
not write.

The answer is `{reply, text?}`. `reply` is what the copilot says — what it
changed, or what it needs to know. `text` is the **whole** field as it now
stands, never a diff. `text` is absent on a turn that asked a question instead
of drafting, which is not a failure: `source` is still `"model"`, and letting a
turn ask is what makes this a conversation rather than a hint box.

**The console owns the transcript; the host stores nothing.** That is the whole
of "in-session" — no journal to rehydrate, no thread id to collide with a desk,
and nothing to clean up when the form closes. It is bounded host-side all the
same (the last 16 turns, 2,000 characters each, blanks and turns with an
unreadable `role` dropped silently), because a transcript the caller composes is
one the caller can grow without limit. A dropped turn is not a `400`: the
transcript is context, not the request, and losing the operator's actual
question over one malformed old message would be the worse failure.

**Neither route writes.** No record is touched, no draft is stored, and no lock
is taken — the response is text, and it becomes a teammate's persona only if the
operator takes it and then saves through `PATCH …/team/{agentId}` like any edit
they typed themselves.

That is the whole reason a model is allowed near these two fields. First-run
setup deliberately keeps the design pass **out** of a teammate's standing
instructions ([company-setup/overview.md](company-setup/overview.md)): the pass
names a work *shape* from a closed enum and the host owns every word, because
there the text would reach a system prompt with nobody having read it, through a
member-open route. Here two deliberate human actions stand in between. If either
is ever removed, this route has to be reconsidered with it.

Grounding is assembled host-side from the company record — this teammate, and
the rest of the roster's ids and roles so a drafted mandate does not restate a
neighbour's. The console holds all of that already and could have sent it; it
must not, because a grounding the caller composes is one the caller can widen.

The exceptions are the fields being authored *right now* — the mandate, the
persona, the role and the name, all of them held by the one form — which the
console does send so that "make it shorter" means shorter than what is on screen
rather than shorter than what was last saved, and so that a teammate repurposed
on screen is drafted for its new job rather than the one it used to do. The role
matters most of the four: both prompts are written *from* it. On the Add form
they ride the request for a second reason, which is that the teammate does not
exist yet and there is nowhere else to read them from.

Every one of those is clamped **on the way in** to the bound the field obeys —
the one-line bound for identity — along with the operator's note and the
conversation, and a value that is blank once trimmed is dropped rather than sent
as an empty string, because "" is not an empty mandate. Nothing else has bounded
them: the request body cap is the only ceiling between a pasted document and the
prompt it would ride in, on every turn of the conversation and on the bill.
Clamping costs a grounding nothing, because text past that bound could never
have been saved into the field anyway.

The answer is clamped to the same bound before it is returned —
`MAX_DESCRIPTION` for a mandate (a card has one line), the persona prompt budget
for instructions — so the console is not the only thing holding the limit.
Drafting is metered as a `SampleKind::AuthoringCall` charged to the **company**,
never to the teammate being described: it ran no turn, and billing it would
otherwise eat that teammate's daily cap. It counts toward the plan-level total
token ceiling (`[plan].total_tokens`) like every other completion the tenant
pays for, and the routes **check** that ceiling before calling a provider — the
same gate the harness applies before dispatch, failing the same way it does: an
unreadable meter warns and lets the draft through, because a metering outage
that silently disabled a working copilot is the worse failure.

Refusals are deliberately not errors. An unknown id is `404` and an unknown
field is `400`, but "no model is wired", "the provider did not answer", "the
answer could not be read" and "this company has spent its budget for the period"
all come back `200` with `source: "unavailable"` and a distinct `reason`
(`no_model` / `model_unreachable` / `unreadable` / `budget_exhausted`), because
each implies a different next move for the operator and none of them is a
failure of the request. Only `model_unreachable` is worth retrying;
`budget_exhausted` in particular is a plan setting rather than a transient
failure, so a shared "try again" would be advice that cannot work.

`unreadable` is narrower than it looks. An answer that is not in the format
asked for is read as a **reply carrying no draft** rather than refused: the
format exists because a draft has to be extracted exactly, and a conversational
reply does not. Only an answer with nothing in it at all is `unreadable`. That
distinction is not theoretical — asked something vague, a model answers with a
plain-prose question about half the time, and refusing those told the operator
their copilot was broken at the exact moment it was doing the right thing. There is no curated fallback text, unlike the roster proposal:
"what does this particular teammate own" has no canned answer, and inventing one
would put words in the company's mouth.

The format is a fence tagged `teammate-field`, not JSON. A persona is a
multi-line document, and escaping one into a JSON string failed on one rich
answer in two — which reaches the operator as a reply with no draft. The block
closes at the **last** ``` in the answer rather than the first, because a
persona that shows worked examples fences them, and closing at the first ```
would hand over a document cut at its first example with nothing on screen
saying the rest had been dropped. The older JSON shape is still read; a ```json
block is explicitly not treated as field text, since handing someone raw JSON as
their teammate's persona is syntactically fine and completely wrong.
