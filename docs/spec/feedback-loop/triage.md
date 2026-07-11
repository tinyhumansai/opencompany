# Triage

How filed feedback becomes shipped improvement, and how the loop closes.

## Label taxonomy

Every feedback issue gets `feedback` plus one from each axis:

| Axis | Values | Meaning |
| --- | --- | --- |
| `type/` | `wrong-output` | the company produced incorrect/bad work |
| | `bug` | runtime misbehavior (crash, lost event, broken route) |
| | `missing-capability` | "it can't do X" |
| | `approval-friction` | the fence is wrong: over- or under-asking |
| | `template-gap` | a template's roster/charter/defaults fall short |
| | `docs` | docs wrong or missing |
| `area/` | `brain`, `runtime`, `product`, `template:<name>`, `integration:<name>` | owning surface |
| `sev/` | `annoyance`, `blocked`, `money-lost` | operator impact |
| `source/` | `operator`, `agent-filed`, `platform` | who filed |

`sev/money-lost` issues page maintainers; `area/brain` issues that reproduce
upstream get mirrored to the owning repo (medulla/backend) with a link, per
the [reuse-first rule](../integrations/README.md).

## Triage flow

1. **Dedupe/cluster** — a triage agent (itself an OpenCompany-style roster
   job, Phase 6) searches existing issues, merges duplicates by commenting
   and closing, and maintains cluster issues ("12 reports: email drafts too
   formal — template:marketing_agency").
2. **Human triage** — maintainers confirm labels, set severity, and answer
   the operator in the thread.
3. **Promotion** — clusters crossing a threshold (count × severity) become
   roadmap items with the cluster issue linked; the threshold and the
   current promotions are public in the tracker, so users can see the
   pipeline they feed.

Agent-filed issues (`source/agent-filed`) carry the filing company's
@handle; repeated low-quality filings from a handle throttle that company's
auto-consent mode.

## Release-notes contract (normative)

- Every release's notes include a **"You said, we did"** section mapping
  fixed issues → the feedback that caused them (issue links, not private
  data).
- On release, the bot comments on each fixed issue; the runtime's update
  check surfaces the in-product notice — *"2 things you flagged were fixed
  in v0.4"* — to the specific operators whose feedback items link those
  issues ([README.md](README.md)).
- A fix without a linked issue is fine; a `feedback`-labeled issue closed by
  a release without appearing in the notes is a process bug.
