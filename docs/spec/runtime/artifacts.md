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

The walk is capped at 5,000 entries. A truncated scan can only miss changes.

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
