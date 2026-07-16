# 07 — WS7: Frontend Wiring (Seam Swaps)

## Scope

Move every console view from its localStorage/sample seam to the real
backend: GraphQL for reads, REST for writes — keeping the graceful-fallback
behavior so the console still works against older hosts.

## Design

### The `gql()` helper

One new method on the existing client (`frontend/src/api/client.ts` — the
only place HTTP happens):

```ts
async gql<T>(query: string, variables?: Record<string, unknown>): Promise<T> {
  const res = await fetch(`${this.baseUrl}/graphql`, {
    method: "POST",
    headers: this.headers({ json: true }),
    body: JSON.stringify({ query, variables }),
  });
  const { data, errors } = await res.json();
  if (errors?.length)
    throw new ApiError(res.status, errors[0].extensions?.code ?? "graphql", errors[0].message);
  return data as T;
}
```

Same bearer/scope resolution as `request()`. Query constants live in
`src/views/queries.ts`; response types are the existing `src/lib/*` models —
the SDL (WS2) was designed to match them field-for-field. New REST write
methods are added to the client per surface, following the existing method
style. No Apollo/urql/react-query; codegen is a later nicety.

### The swap pattern (per view)

Each view keeps its three-state seam (`loading | ready | unavailable`), but:

1. read: `client.gql<…>(QUERY, { id: company })` replaces the localStorage
   load;
2. writes: optimistic UI + REST call + toast-on-error (pattern already used
   by Team/Connections);
3. the sample/seed module is **deleted** (or reduced to the fallback shown in
   the `unavailable` state);
4. the `fromHost`/`unavailable` copy stays so operators know when data is
   local.

localStorage remains only where it is genuinely client state (theme, token,
last-selected desk) — not for domain data.

### Per-view checklist (one commit each, in backend-landing order)

| View | Read | Writes | Deletes |
|---|---|---|---|
| Team | `company.team` | POST/DELETE `…/team`, PUT `…/team/{id}/inbox` | `starterTeam()` fallback trimmed |
| Conversation | `company.chats` + `chat.history` | `POST …/chat {message, chat}` | `defaultThreads()` sample |
| Skills | `company.skills` + `skillRegistry` | install/uninstall/enable/custom | `SKILL_REGISTRY`, `seedSkills()` |
| Workflows | `company.workflows` / `workflow(id)` | — (read-only canvas) | `SAMPLE_WORKFLOW` |
| Workspace | `workspaceTree` / `workspaceFile` | POST/PUT/PATCH/DELETE `…/workspace` | `seedWorkspace()` + localStorage store |
| Tasks | `company.tasks` | POST/PATCH/DELETE `…/tasks` | `sampleTasks()` |
| Memory | `company.memory` | POST/DELETE `…/memory` | `seedMemory()` |
| Usage | `company.usage(range)` | — | `buildUsage()` RNG sample |
| Finances | `company.finances` | — | `buildFinance()` sample |
| Connections | `company.connections` | start/disconnect (existing methods) | catalog stays (it's static UI data) |
| Settings (domain/SMTP) | `company.domain` / `company.smtp` | PUT domain, POST verify, PUT smtp, POST test | localStorage `oc-mail` draft |
| Inbox | `company.inboxes` + messages | POST `…/inboxes/{key}/read`, toggle via Team | `seedInboxes()` |

Each commit also updates the matching row in `frontend/ARCHITECTURE.md`
(seam/client-only → real) — that file stays the live contract map.

### Tests (new Vitest setup — see 09-verification.md §3)

- `src/api/client.test.ts` grows a contract test per new method (URL, verb,
  body, error shaping) plus `gql()` error unwrapping.
- Pure-logic lib tests (workspace tree ops, wikilinks, language) land with
  the Vitest bootstrap commit, before any view swap.
- `npm run typecheck && npm run build && npm test` green per commit.

## Subtasks

1. `feat(console): vitest bootstrap + client contract tests`
2. `feat(console): gql() helper + queries.ts scaffold`
3. Per view, one commit: `feat(console): wire <view> to the host` — in the
   order the backend lands (Team/Conversation/Skills/Workflows first after
   WS2a/b; Workspace/Tasks/Memory after WS2c+WS3; Usage/Finances after WS5;
   Connections/Settings/Inbox after WS6).

At most 2–3 subagents in parallel (shared `client.ts`/`types.ts` — the
merge-conflict hotspot; registration edits land serially).

## Dependencies

Trails WS2/WS3/WS5/WS6 surface-by-surface. Subtasks 1–2 only need WS2's
foundation commit.

## Tests & exit criteria

Per view: sample module deleted, fallback retained, typecheck+build+vitest
green, manual pass against a live host running
`companies/agentic_marketing_agency`. Playwright flows follow once the
majority of views are wired ([09-verification.md §4](09-verification.md)).
