# Deliverables

What a task *produced*, as opposed to what its agent *said*.

An artifact is a versioned record of a file an agent wrote and explicitly
handed over. The port that stores it
([`ArtifactStore`](ports-console.md#artifactstore)) exists for
one question: **how much did the human have to fix the agent's work?** That is
the highest-signal quality datum the product can produce, and it is computable
only if the record is honest about what it holds.

## The rule

> An artifact exists **iff** the agent called `publish_artifact` on a file
> inside its own workspace, during a run that reached its success terminal.

A run that publishes nothing yields **no artifact**. That is a first-class
outcome, not a gap: plenty of tasks — a question answered, a check run, a
decision made — produce no file, and the addressable record of what happened is
the run trace. An empty Artifacts tab means nothing was produced, never that
something was lost.

### Two things this is not

**Not an auto-sweep.** The agent sandbox also hosts exec-grade shell and code
tools, so it routinely contains repositories, package caches and build output.
Promoting whatever changed would flood the deliverable list with junk and make
the churn metric meaningless. An explicit call also carries intent — a title, a
kind, a note — that a sweep cannot invent.

**Not reply capture.** Before this rule, every completed dispatch recorded its
chat reply as an artifact, gated on run disposition and never on content. An
agent replying *"I can't do this, I'm blocked on the API key"* produced a
versioned record indistinguishable from a real draft, so the tab presented
refusals and blocker messages as deliverables. Reply capture is **removed**, not
demoted. Nothing is lost by removing it: the reply still lands in the chat
bubble, the timeline event, the terminal anchor, the card note and the run
trace — five records, none of which claims to be a deliverable.

**There is no content classifier anywhere.** Detecting a refusal by inspecting
prose was considered and rejected: it is a guess about meaning, it fails
silently in both directions, and the honest signal — *did the agent publish
anything?* — is already exact.

Refusals and blockers go where they already live: the timeline, the card note,
the run trace. Never into an artifact.

## Identity

A record carries a `source`: the normalized workspace-relative path it was
published from, e.g. `specs/launch.md`. Together with the task id that is the
artifact's identity.

- Republishing the same path on a later attempt **appends a version** to the
  same record.
- Publishing a different path **opens a new record**.
- Nothing else selects the record a revision extends.

The last point is a correction, not a refinement. The extend target used to be
whichever artifact on the card had the newest `updated_at_millis` — and an
*operator edit* bumps that. So an operator who tidied the invoice made the
invoice the target for the agent's next write to the spec: the spec's v2 landed
as the invoice's v3, and the human-edit diff then reported a person rewriting a
document they had never opened. Since that diff is the entire purpose of the
port, recency selection did not merely mis-file records; it fabricated the one
number the product is trying to measure.

**A rename starts a new lineage.** Tracking moves would need content hashing or
a rename hook the file tools do not have, and would guess wrong exactly when two
drafts are similar. A new record for a renamed file is honest; a wrong merge is
not.

## Legacy records: `source == None`

Every record minted under this rule has a `source`, because the only way to mint
one is to publish a file. So **absence marks a pre-existing reply-capture
record**.

Those records are kept — nothing rewrites the past — and there is no migration.
The console labels them *"chat reply (legacy capture)"* and says plainly on the
detail view that they record what was said rather than a file that was produced.
The honest consequence: a company that ran before this change still has refusal
messages in its artifact list, now visibly demoted rather than silently
masquerading as output.

## Where it lands in the shared tree

The record is one half; the other is a node in the company workspace, so a
deliverable is something an operator and every other agent can navigate to
rather than a row behind one card's Artifacts tab. `company::artifact_mirror`
files it at:

```text
artifacts/<agent-id>/<task-title>.<task-id>/<source…>
```

`artifacts/` is an eagerly-scaffolded system root carrying a `readme.md`; the
member folder beneath it is minted the first time that agent publishes
(`workspace_scaffold::ensure_artifact_folder`), so the list under it is a record
of who has delivered rather than a copy of the roster.

### The task folder is named for the work and keyed by the id (issue #1687)

The folder used to be named by the card id alone — a good key and a useless
label, since `artifacts/cmo/` was a column of `01hq8zm4x…` that could only be
told apart by opening each one. It now carries the card's title first, because
the console's tree truncates from the right, and the card id last, because that
is the only half that is unique and immutable: two cards titled "Weekly update"
must not share a folder, and an operator holding a card id needs something in
the tree to match it against. The title half is budgeted against the 96-byte
name cap so the id half is never truncated, and a title that normalizes to
nothing (an emoji, punctuation) leaves the folder named by the id alone rather
than collapsing every such card onto `untitled`.

**The join is a dot, and the lookup is on the id half, not on the name.** A
title is editable, so an exact-name lookup would stop finding the folder the
moment somebody retitled the card and the next publish would mint a rival
beside it, splitting one task's deliverables across two folders. The lookup
therefore reads the text after the name's last dot and compares it to the card
id. The dot is what makes that an equality test: a seed card's id is
`[a-z0-9-]` (`task_file::normalize_task_id`), so `login` and `fix-login` are
both legal ids and a dash join would leave one card's folder ending in the
other's id — while neither id grammar can produce a dot. The same lookup
**adopts** a folder minted before this change, whose name is the bare id.

**Nothing is renamed**, here as everywhere else in this runtime: an existing
folder keeps the name it was minted under, and only a task publishing for the
first time gets a titled one.

It used to be `agents/<agent-id>/<task-id>/…`, which filed a deliverable in the
same folder as its author's scratch notes — the two populations were
indistinguishable by eye, and "what has this company produced?" had no answer
that was a place. Filing by kind first and author second keeps the attribution
and makes the deliverable list navigable.

**Nothing migrates.** A record that already carries a node id keeps revising
that node, so a company that published before this change keeps its existing
nodes and every console deep link into them; only new paths land under
`artifacts/`. Moving nodes an operator may have organised by hand, to fix
something untidy rather than wrong, is the worse trade.

The node is a **projection**: it holds the current body, while the artifact
chain remains authoritative for the version history and for
`human_edit_diff`. The invariant, the write ordering that protects it, and what
an operator edit to one of these nodes records are all in
`src/company/artifact_mirror.rs`.

## Bodies, caps and references

`MAX_ARTIFACT_BODY_BYTES` is 256 KiB.

| File | Stored as |
| --- | --- |
| UTF-8 text at or under the cap | the content, in full |
| over the cap, or not UTF-8 | a structured **reference** |

A reference body names the workspace-relative path, the exact byte size and the
sha256 of the bytes. It is never silently-truncated content presented as
complete — a half-spec that looks finished is worse than an honest pointer.

The body is computed **at publish time**, not when the run drains its queue. A
later shell step that rewrites the file cannot retroactively make the tool's
success message a liar.

The digest is what makes a reference verifiable afterwards. Its honest
limitation: the payload's durability is the workspace's durability. Wipe the
sandbox and the record survives while the reference dangles. A blob store with
GC is a follow-up.

## The one nudge

Agents forget. So after a run ends `Completed`, the harness compares a
dispatch-start snapshot of the workspace against its current state and subtracts
what was staged for publication. If anything is left, it runs **exactly one**
follow-up turn naming those files.

Properties that are load-bearing:

- **One, structurally.** The nudge is straight-line code guarded by a local, not
  part of the redirect loop and not bounded by a counter. A second nudge has
  nowhere to be written.
- **It carries its own context** — the original brief, the agent's completed
  reply, the file names — because it is a separate turn.
- **It is non-coercive.** The prompt offers the decline in the same breath as
  the publish and says outright that scratch files and unfinished work are a
  fine answer. A nudge that reads as an instruction produces published build
  logs.
- **A decline is a clean outcome.** The reason is appended to the card note as
  `unpublished: <files> — agent: <reply>` and nothing further happens: no
  artifact, no error, no retry.
- **It cannot fail the run.** A provider fault or a refused budget logs a warning
  and falls through to the fallback. The run's work was already done and its
  reply already decided.
- **The fallback warning uses the pre-nudge diff**, so a scratch file written
  *while answering* cannot become evidence against the agent.

It is skipped entirely when the responder was reassigned mid-run (a hand-off),
because the snapshot then pairs with the wrong workspace.

**Cost.** The nudge is one extra model turn on the same path, so its spend lands
in the same usage ledger and counts against the agent's daily budget. It is
distinguishable in the run trace — its steps append to the same attempt after
the primary reply — but it is **not separately labelled in the usage ledger**.
Adding a provenance field to `UsageSample` is a real change to the metering
surface and is deliberately out of scope.

## What the scan ignores

The unpublished-file scan is a **detection aid feeding a warning, never a
promotion**, so under-reporting is the acceptable failure direction and
over-reporting is not.

It skips dependency and build trees (`node_modules`, `target`), everything
hidden (any name starting with `.`), and the runtime's own bookkeeping —
`sessions/`, `session_raw/`, `artifacts/`, `checkpoints/`, `tinyagents_store/`,
`audit.log`. That last group is not a tuning detail: the agent's workspace is
*also* where OpenHuman writes session transcripts and TinyAgents writes its
message journal, on every single run. Counting them would fire the nudge after
every dispatch, asking an agent whether its own transcript is a deliverable.

`audit.log` is a leftover of that list rather than a live member of it. The shell
audit sink moved out of the workspace to the host-owned
`companies/<slug>/audit/<agent>/` in issue #775 — see
[agent-isolation.md](../security/agent-isolation.md) §6 — so a workspace created
after that change never contains one. The skip stays for workspaces provisioned
before it, and because the name is a plausible one for something else to write.

The walk is capped at 5,000 entries. A truncated scan can only miss changes.

## A workflow run cannot file a deliverable, and now says so (issue #1192)

Publishing needs somewhere to record the version. The chat and task paths each
claim a destination before the turn (`PublishDestination::Conversation` /
`::Task`); a **workflow run claims nothing**, because `publish_artifact` needs a
card to attach a version to and a run has neither a card nor a conversation. So
a node that calls the tool is refused in-turn, and that refusal is correct —
`Unclaimed` is the `#[default]`, which is the fail-safe direction: a turn-running
path added later inherits an honest refusal rather than a silent drop.

What was wrong was who heard about it. The refusal was told to the model and to
`tracing::warn!`, and to nobody else. The model wrote an apology, the apology
became the node's `text` output, the `=items` binding delivered it downstream as
though it were the deliverable, and the run settled clean — the same shape issue
`#881` fixed for the *gated* case, which the `Unclaimed` case never got.

The refusal is now a **typed fact recorded where it is raised**: a second bucket
on `PendingPublishQueue` (`push_refusal` / `drain_refusals`), drained after every
agent-node turn into one deduped `WorkflowRun::notices` line naming the path.
Three properties are load-bearing:

- **Recorded at the raise site, never classified from prose.** Matching on the
  refusal sentence would be a classifier keyed on agent-facing copy: the wording
  will be reworded, and the day it is, the notice silently stops appearing with
  every test still green.
- **A notice, not a `Blocked` node.** A refused publish did not stop the node —
  the turn ran and the branch continued. `Blocked` halts a branch and is not
  auto-resumable, and there is no approval here for anyone to give.
- **A refusal is not a `sources()` entry.** The unpublished-file scan above is
  `changed − sources()`, so counting a refusal as staged would make the nudge go
  quiet on the file *most* at risk — the one an agent explicitly tried and failed
  to hand over. The two buckets are separate for exactly this reason, and
  `clear()` (the steer-redirect abandon) empties both.

Whether a run *should* be able to publish is a separate, open question:
`origin_run_id` (M5 / issue #661) taught runs to open cards, which arguably makes
the "a run has nowhere to file one" premise stale. This change does not settle
it — it only removes the silence around the current answer.

## Storage

Versions are append-only and each republish appends a full body. The whole
record is one JSON row or document, so MongoDB's 16 MB document cap is the hard
wall — roughly 60 max-size versions per artifact. The per-version cap bounds the
worst case; delta storage and pruning are a named follow-up.

`source` is additive on the wire (`#[serde(default, skip_serializing_if)]`) and
all three backends persist the record as an opaque JSON blob, so this needed no
schema migration on any of them.

## Related

- [ports-console.md](ports-console.md#artifactstore) — the `ArtifactStore`
  trait contract
- [events.md](events.md) — `DeskTaskCompleted.artifact_ids`
