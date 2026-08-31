# Workflow authoring routes — copilot & task-card proposals

Split out of [workflow-routes.md](workflow-routes.md) (issue #695 line-cap).
These are the **authoring** surfaces — building a graph from a task card,
drafting one from a description, and grounding either on the tools a company
can actually reach. The read/edit/run and delivery routes stay in the parent.

## Building a workflow from a task card (issue #580)

A board card marked `deliverable: "workflow"` does not dispatch to a teammate
when it enters In Progress — it builds a *reusable workflow* instead. The builder
pass (`src/harness/workflow_build.rs`) proposes a graph and lands the card **In
Review** with a `TaskWorkflowProposal`; the graph does not exist yet. Two task
routes finish the loop:

- `POST …/tasks/{id}/workflow-proposal/apply` rebuilds a `RawWorkflow` from the
  **stored** proposal `ops` (host authority — the browser's copy is never
  trusted) and runs it through the **same** `create_company_workflow` core this
  page's `POST …/workflows` uses, so a proposed graph passes exactly the checks a
  hand-authored one does — including #276's create-disarm for a scheduled graph.
  On success the card links to the created workflow (issue #339) and moves to
  Done; a refused create (roster drift, a name taken since) keeps the card In
  Review with the reason and returns a 400.
- `POST …/tasks/{id}/workflow-proposal/reject` clears the proposal and returns
  the card to To-do.

The full contract — the deliverable choice, the builder pass, and the
review-before-creation gate — is [workflow-build.md](../../spec/runtime/workflow-build.md).

## Drafting a workflow from a description (issue #753)

`POST …/workflows/draft-from-description` is the New-workflow dialog's copilot: it
turns a sentence into a graph the create form loads, so an operator can start
from a description instead of a blank form. It is the same engine as the #580
card builder (`draft_workflow_from_description` in `src/harness/workflow_build.rs`)
with the board card removed — the company evidence, the one tool-less model call,
and the host's authority over the id, the display name, the approval gating and
the node-kind vocabulary are identical. The one extra it grounds the model in is
the company's **effective tool slugs** (`workflow_effective_tool_slugs`), because
a typed description is far likelier to want a `tool_call` step than a card is.

**It never persists.** The draft is validated exactly as `POST …/workflows`
would (`courtesy_validate_draft`), handed back, and hydrated into the create
form; the operator reviews and edits it there and presses Create, which is still
the only call that saves a graph. So a bad draft costs a review, not a rollback,
and the review-before-creation discipline the card builder keeps is preserved
without a board card.

Like the cron preview, it answers **200 in both model-answer cases** — a drafted
graph, or an honest "this is better done once" — keyed by `automatable`:

```bash
curl -X POST "$HOST/api/v1/company/workflows/draft-from-description" \
     -H "Authorization: Bearer $TOKEN" \
     -H 'content-type: application/json' \
     -d '{"description":"Every Monday, have the writer draft the weekly digest and email the team."}'
```

```jsonc
{ "automatable": true,
  "summary": "Draft and email the weekly digest every Monday",
  "workflow": { "id": "weekly-digest", "name": "Weekly digest", "nodes": [ … ], "edges": [ … ] } }
```

```jsonc
{ "automatable": false,
  "reason": "this is better done once than built into a workflow: it names a one-time cleanup" }
```

An empty description is a `400`. A build with no embedded brain classifies the
gap the way the run route does — `not_wired` (404), `restart_required` or
`inference_required` (409) — so the console points the operator at the same next
step (a restart, or configuring inference in Settings) rather than a bare
failure. The spend is metered like a card pass, under a freshly minted id and a
`workflow:copilot` sentinel agent; there is no `RunStore` row, because a
synchronous request is not a card's attempt at its own work.

## Which tools a proposal may name (issues #783, #874)

`GET …/workflows/tool-slugs` is the browser-side copilot's tool grounding — the
`CopilotPanel` reads it once and inlines the answer in the message it composes,
so a proposed `tool_call` names a real slug instead of an invented one.

```jsonc
{ "slugs": ["shell", "read_workspace_state"],
  "unwired": [ { "slug": "web_search",
                 "reason": "searchBackendNotConfigured",
                 "detail": "granted, but no managed search backend is configured on this deployment; …" } ] }
```

`slugs` is the **effective** set — `workflow_effective_tool_slugs`: the catalogue,
the company's `[tools].allow`, and this deployment's wiring all agreeing. It is
the same set the in-process create/fix copilot grounds on, so the two surfaces
cannot drift.

`unwired` is the granted-but-unwired remainder, with the reason from the same
`WorkflowToolWiring` the run-time gate reads — `searchBackendNotConfigured` or
`capabilityTierFiltered`, matching the two sentences `refusal_for` produces at
run time. Reporting it, rather than dropping it, is what lets a reader tell "this
company is not allowed that tool" (absent from both lists) from "allowed, but
nobody configured the provider here".

That distinction is issue #874. The route used to answer the wider **grant-only**
set, so on a deployment with no search credential a granted `web_search` was
offered, the copilot authored a node on it, and the run failed at the first node
with `tool_call 'web_search' is not available in company workflows`.

Two deliberate non-changes:

- **Create/save validation stays permissive.** `validate_tool_call_node` still
  checks grants alone, so authoring a graph now and wiring the provider later
  remains legal. This route narrows what a caller is *told is available*, not
  what the host will *accept*.
- **Unknowable wiring is not "unwired".** With no harness deps attached the
  deployment cannot be asked, so `slugs` falls back to the grant-only set and
  `unwired` is empty — the pre-#874 answer. Claiming every granted tool is broken
  would be the worse failure. A default build (no `openhuman` feature) wires no
  `tool_call` grants at all and answers two empty lists rather than a 404, so the
  copilot grounds on "no tools" instead of being unable to tell.

A host predating #874 sends no `unwired` key; the client defaults it to `[]`,
which reads identically to a fully wired deployment.
