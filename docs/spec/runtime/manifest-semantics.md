# Company Manifest: Semantics

The behaviour each `company.toml` key and table actually has, beyond the
schema sketch in [manifest.md](manifest.md). Split out from that file to keep
each page under the 500-line cap.

## Semantics

- **`[company]`** becomes the seed of the [Charter](../company-brain/charter.md).
  `handle` is only used when `[place].discoverable = true`.
- **`[[agent]]`** entries define the Roster — or, equivalently, one
  `agents/<id>.toml` file per teammate under the company bundle, carrying the
  same keys with the filename as the id. The two forms are **exclusive**:
  declaring both is a validation error rather than a precedence rule, because
  either precedence would silently discard teammates somebody wrote down. Full
  schema, including `prompt` / `prompt_files` / `context` / `classes`, in
  [runtime/agents.md](agents.md).

  `tier` is a hint the brain may use when delegating; it never selects a model
  (the backend maps tiers to SKUs). `tools` and `budget_usd_daily` intersect
  with the company-wide `[tools].allow` and `[budget]` — the most restrictive
  wins. Tool grants resolve through **three** levels,
  `[tools].allow ∩ [[group_chat]].tools ∩ [[agent]].tools`, every one of them
  narrow-only and an absent one a pass-through; see
  [runtime/tools.md](tools.md), which also covers why an absent grant means
  "inherit" rather than "nothing" — and why, since #1804, an **explicit empty**
  agent `tools` list (`[]`) is a deliberate deny-all rather than an inherit.

  **`delegates_to`** (issue #176) is the one per-agent key that is *not* a
  narrowing of a company-wide list: it is an **opt-in**. Empty — the default,
  and every manifest written before it existed — means the agent carries no
  delegation tool at all, which is how a dispatched desk agent has always
  behaved. Naming one or more desks wires exactly two tools onto it,
  `spawn_task` and a `delegate_to_desk` narrowed to those desks, so a desk lead
  can pull a specialist in for one slice instead of handing the whole request
  back to the orchestrator.

  It takes **desk** ids or names (`[[group_chat]]` entries), never teammate
  ids — desks are the address space `delegate_to_desk` already resolves
  against — and `"*"` means every desk the company has. An entry that names no
  declared desk fails validation, because at runtime it would fail silently:
  the member would carry the tool and every call would be refused.

  It never confers the orchestrator's *authority* — `assign_task`,
  `review_task`, `add_agent`, `query_company`, `run_workflow`,
  `create_workflow`, and the #661 workflow-admin trio (`read_workflow`,
  `update_workflow`, `delete_workflow`) stay orchestrator-only. A member gets what it needs to pass
  a slice on and to leave the rest tracked, and nothing more.

  Three runtime guards bound what it can do, all enforced at the tool boundary
  in the member's own turn rather than by which tools were wired (belts are
  cached per roster, so a tool cannot be withheld from one turn):

  - **Depth** — `[tools].max_delegation_depth`, below.
  - **Cycles** — a hand-off to a desk already on the current chain (A→B→A), or
    to the desk the caller itself leads, is refused.
  - **Allowlist** — a target outside `delegates_to` is refused, and the refusal
    names the desks the member *can* reach so it can retry in the same turn.

  Each refusal reaches both the model and the board: the run trail carries it
  verbatim, and a refused hand-off is recorded on the dispatched card's note,
  so the operator reads the fact rather than inferring it from an absence.

  The per-turn fan-out cap (three delegations) applies **per level**, not per
  message — each turn starts against an empty queue.

  **`budget_usd_daily`** (enforced since issue #304 — before that it was
  validated, stored and displayed, but nothing read it) caps one teammate's
  spend over the **UTC calendar day**, resetting at `00:00Z` — the same
  boundary `[plan]`'s daily token budget uses. Spend is re-read from the usage
  meter on every check, so a restart mid-day resumes against the real figure
  rather than a fresh zero.

  It covers **metered, attributed** spend: inference turns and priced tool
  calls (`web_search`, `media_generate_*`), plus any tool call that declares an
  `amount_usd`. Two behaviours at the cap:

  - **Dispatch is refused** for that teammate, before any model call, with a
    notice naming the cap and the reset. The rest of the company keeps running.
  - **Priced tool calls are not converted into HITL prompts.** While policy
    HITL is disabled, dispatch is the enforced cap boundary; calls later in an
    already-running turn may overshoot it within that turn.

  Known limits, stated rather than papered over:

  - **Executed x402 payments escape the counter.** Ledger entries carry no
    agent, so there is nothing to attribute them to. What *is* covered is the
    pre-flight case: a call declaring an `amount_usd` that would breach the
    remaining budget parks before the money moves. Company-wide payment
    spending is governed by `[budget].monthly_usd`, which is enforced on the
    economy path.
  - **Turn-boundary overshoot.** A turn or call that starts under the cap can
    finish over it; the overshoot is bounded by one call. There is no
    reservation ledger in v1 — the same documented window `[plan]` carries.
  - **Unreadable spend fails differently at each layer, deliberately.** With no
    meter or a failing meter, dispatch **runs** (bricking a teammate's
    cognition with no operator recourse is worse than a day of overspend),
    while an already-running priced tool call is not interrupted by HITL.
  Since issue #343 the manifest value is a **default, not the last word**. An
  admin can set, change or clear a teammate's cap from the console
  (`PUT`/`DELETE …/team/{agentId}/budget`); the override is stored on the
  company record, wins over `budget_usd_daily` everywhere the cap is read
  ([`CompanyRecord::effective_budget`] is the single reconciliation point), and
  takes effect on the teammate's **next dispatch** — no restart, no redeploy.
  That matters most on a hosted tenant, where `company.toml` is baked into the
  container image and an operator has no way to edit it. Three stored states,
  kept deliberately distinct: no override (the manifest applies), a cap of `x`
  (`0` included, meaning "may not spend"), and an explicit "uncapped" that beats
  a manifest cap. `DELETE` drops the override so the manifest applies again,
  which no `PUT` body can express. Writes are admin-only and record who set the
  cap and when.

  - **Operator-added (overlay) teammates carry no manifest cap**, because they
    have no `[[agent]]` entry — but since #343 they can be capped through a
    console override like anyone else, including at creation.

  [`CompanyRecord::effective_budget`]: ../../../src/ports/types.rs
- **`[brain]`** selects the `Brain` implementation. `hosted` requires a
  TinyHumans credential at runtime; `sidecar` requires the `sidecar` feature.
- **`[[harness]]`** declares the company's named execution engines; the full
  story is [harnesses.md](harnesses.md). Each entry has a unique `id` and a
  `kind` (`built_in` | `acp`), and exactly one sets `default = true`. An agent
  binds with `harness = "<id>"`, or takes the default. **Absent entirely** — the
  case for every bundle under `companies/` — means one implicit `built_in`
  harness on the company-level `[inference]`, so the table is purely additive. A
  section on the wrong kind (`[harness.inference]` on an `acp` entry, or
  `[harness.acp]` on a `built_in` one) is a validation error, not an ignored
  key.
- **`[inference]`** (issue #56 — BYOK) routes agents through a chosen model
  provider, and is the fallback for a harness declaring no
  `[harness.inference]`. `provider` is one of `openrouter` (the default) /
  `openai_compatible` / `ollama`; `base_url` is required for the latter two.
  `api_key_secret` names a **secret-store key** — never the token, which is
  written write-only through the console (Connections → Inference).
  `[inference.models]` maps an abstract cognition tier (`chat-v1`,
  `reasoning-v1`, `agentic-v1`, `vision-v1`) to a concrete OpenRouter model id.
  `openrouter` is **dual-mode** — keyless rides the subscription, a tenant
  `sk-or-…` goes direct — and the removed `managed` kind aliases to it. Full
  detail, including the per-harness secret slots, is in
  [providers.md](providers.md).
  Precedence is **runtime console override > manifest > platform
  default**, and a per-tenant provider re-resolves it every turn — so a console
  switch takes effect on the agents' next turn with **no restart**.
  That holds only once the company is already on the harness cognition path.
  *Which brain a company runs* is decided once, when the runtime is built: a
  company that resolved no inference source at boot gets the offline echo brain,
  and a credential saved afterwards does not reach it. The status route reports
  that as `restartRequired` (issue #266). Since issue #290 the save **rebuilds
  the runtime in place** rather than asking for a restart a hosted operator has
  no way to perform — see [runtime rebuild](rebuild.md). `restartRequired` stays
  honest: still `true` on a host that wired no rebuilder.
  Saving the platform default from the console is a *revert*
  (`DELETE …/inference`) and
  carries no credential, so the console refuses that save while a key is still
  typed in the form rather than dropping it and reporting success (issue #265).
- **`[channels.*]`** enables `ChannelAdapter`s. Unknown channels are a
  validation error; disabled OpenHuman means non-operator channels degrade
  with a boot warning, never a failure.
- **`[policy]`** retains the autonomy vocabulary and stored configuration, but
  policy-generated HITL is currently disabled. `supervised`, `auto`,
  `always_approve`, and `auto_approve_under_usd` do not manufacture cards;
  agents ask explicitly with `request_approval`. `readonly` remains a hard
  denial for mutating or external agent tools. `approval_ttl_hours` sets
  how long a parked approval waits before it default-denies (24 hours by
  default — see [approvals.md](../company-brain/approvals.md), issue #971).
  Omitting it is not the same as writing `24`: the key stays absent from the
  persisted seed, which is what keeps a future change to the default from
  looking like an edit and discarding a console `[policy]` override. The parse default is
  `supervised`, but a **new**
  company is given `auto`, written into its manifest explicitly rather than
  left to that default. See
  [grants.md](../company-brain/grants.md#which-tier-a-new-company-gets)
  for why those are two separate knobs, and why moving the parse default is the
  one thing issue #605 declined to do. **A tool name is an
  effect kind** — the harness projects one onto the other — so
  `["publish_artifact"]` and `["payment.send"]` are the same syntax at
  different segment counts (issue #684). Operator-authored effect kinds remain
  open-ended because a hosted brain may emit a kind this repository has never
  seen; the shared matcher runs before the checkpoint taxonomy. The default is
  **empty**: `supervised` already parks every money / publish / filing effect
  through that taxonomy, so the conservative default is the mode, not the
  list. Existing values remain loadable for historical approvals and future
  policy modes.
- **`[place]`** drives the [going-public flow](../company-as-agent/README.md).
  `skills` feed Agent Card generation; prices are decimal strings (USDC).
- **`[budget].monthly_usd`** is a hard ceiling enforced by the kernel across
  inference usage and x402 spend; reaching it pauses the company with an
  operator notification rather than silently degrading.
- **`[plan]`** (issue #108) gates the exec tool families (`shell`, `code`,
  `web`, `subagent`) by the company's **token spend this period**, a distinct
  axis from `[policy].mode` (autonomy) and an agent's `tier` (cognition). A
  built-in `name` (`free` / `starter` / `pro` / `unlimited`) supplies a base
  budget map; `token_budgets` overrides/extends it per namespace. The map's key
  set **is** the capability set — a gateable namespace absent from it is denied
  outright. Each budget is a **threshold over total period token spend**, not a
  per-namespace meter (usage samples carry no per-tool attribution): when spend
  reaches a tier's budget, that tier's tools switch off for the rest of the
  period; different budgets give **graduated degradation**. `period` is `daily`
  (default) or `monthly`, aligned to UTC calendar boundaries. Gating is
  **fail-closed** — if the usage meter can't be read, every gateable family is
  denied (the turn still runs on its intrinsic memory/file/MCP tools). The gate
  re-resolves before every turn, so a tier that crosses its budget mid-session
  switches off on the **next** turn (a turn already in flight finishes). An
  absent `[plan]` leaves gating off entirely. The console's Usage view shows a
  live per-tier budget card (`GET …/capabilities`).
  - **`media`** (issue #109) is a fifth gateable namespace covering the
    image/video generation tools (`media_generate_image`,
    `media_generate_video`, `media_list_models`), but it is **real-money and
    opt-in**: it is granted only by an **explicit** `media` / `media.*` entry in
    `[tools].allow` — the `*` wildcard deliberately does **not** grant it — and
    it runs exclusively on a **platform credential** (resolved from the
    environment, never a tenant BYOK key or secret). It is absent from the
    `free` / `starter` / `pro` tiers (denied there) and uncapped only under
    `unlimited`; a company opts in per-namespace with
    `token_budgets = { media = N }`. Every generation additionally **parks for
    operator approval** before the backend bills it, and the whole family is
    compiled out unless the build enables the `media` feature. With no platform
    credential configured, a `media` grant wires no tools (fail-closed). The
    Usage view surfaces a dedicated media status row (active / awaiting
    credential / not granted / not in this build).
  - **`search`** (issue #238) is a seventh gateable namespace covering the single
    `web_search` tool — source *discovery* for the research skills, which
    previously ran on a belt that could read a known URL but never find one. It
    is **priced and opt-in**: granted only by an **explicit** `search` /
    `search.*` entry in `[tools].allow` (the `*` wildcard deliberately does
    **not** grant it, and unlike `media`/`composio` it is **not** in the default
    grant list either), and it runs exclusively on the **platform
    credential** — the same identity as keyless `openrouter`, resolved from the
    environment, never a tenant key. The backend charges per request and reports
    the amount, which is recorded as one `SearchCall` usage sample and rolls into
    the window's cost.
    Three things differ from `media` on purpose:
    - **Individual searches do not park for approval.** Consent is the explicit
      grant; the boundary is `[tools].search_daily_calls`, a per-company **daily
      call cap** (default 200; `0` pauses search without editing `allow`).
      Over-cap returns a loud "search budget exhausted" tool error, never an
      empty result set — an agent handed silence invents citations. An operator
      who does want a per-call gate sets
      `[policy].always_approve = ["web_search"]`, which overrides every tier.
    - **`[policy].mode = "readonly"` still denies it.** A search reaches a third
      party and spends money, so a desk whose contract is that nothing is spent
      does not get one.
    - **There is no `search` Cargo feature.** The tool rides the `openhuman`
      harness feature so CI's gated lane actually compiles and tests it.

  **`max_delegation_depth`** (issue #176) bounds how deep one operator
  message's hand-off chain may run, counted in hand-offs: the orchestrator
  handing work to a desk lead is level 1, that lead handing a slice on is
  level 2. Default `2`; valid `1..=4`, where `1` is the "recursion off" setting
  and reproduces the pre-#176 behaviour exactly.

  The depth in force is read from the **live company record** on every call, so
  lowering it takes effect on the next turn without a rebuild. A hand-off past
  the bound is refused in the model's own turn with the reason
  `depth_capped` — while `spawn_task` still works at the bound, so a member that
  has run out of chain leaves the remaining work tracked instead of doing it
  silently.

  The bound only ever matters to an agent some manifest opted in with
  `delegates_to`; a company that names nobody is unaffected by any value here.
  It is deliberately low: the fan-out cap applies per level, so each extra
  level multiplies the turns one message can buy.
    The Usage view surfaces a `Web searches` KPI plus a search status row
    (active / paused at cap 0 / awaiting credential / not granted / not in this
    build).
  - **`workspace`** (issues #237, #551, #671) grants the company's shared note tree —
    the `standards/` / `playbooks/` / `product/` documents seeded from
    `companies/<name>/workspace/**`, plus whatever the operator and the agents
    have written since. It is **split**, unlike every other namespace: *reads*
    (`workspace_list`, `workspace_search`, `workspace_read`) follow the
    ordinary rule, so a
    catch-all `*` confers them; *mutations* (`workspace_write`,
    `workspace_create`, `workspace_rename`, `workspace_delete`) need an
    **explicit** `workspace` or `workspace.write`
    entry in `[tools].allow`, because they change a tree every other agent then
    treats as the company's source of truth. `workspace.read` is therefore a
    genuinely read-only grant. All four ride the one flag on purpose:
    overwriting an existing standard is strictly more destructive than adding a
    note beside it, and strictly more destructive than removing or moving
    something inside the agent's own folder, so a grant permitting the first has
    already permitted the rest. Issue #671 deliberately added no fifth grant
    name. `workspace_search` (issue #607) is a read and rides the read side of
    that split — **not** the metered `search` namespace, despite the name.
    `search` is the paid external-credential grant that carries `web_search`;
    reading the company's own notes must not require a billed credential, and
    search reads exactly what `workspace_read` already grants, so it costs the
    operator no additional decision. `workspace_write` overwrites one
    **existing** note and requires an
    `expected_updated_at` revision token taken from a prior read, so a note
    edited in the console since the agent read it is refused rather than
    clobbered. `workspace_create` adds one folder or note at a path that is
    **free** and whose parent folder already exists — never an overwrite, never
    a `mkdir -p`. The single exception is the agent's own
    `agents/<agent-id>/`, which is created on demand when the agent writes
    directly into it, because that folder is minted on first use rather than
    provisioned at boot. Since issue #552 this call is one of two paths that
    bring it into existence; publishing a deliverable
    (`artifact_mirror::materialize`) is the other, and both go through the same
    `ensure_agent_folder` seam.

    `workspace_rename` and `workspace_delete` (issue #671) are the tidying half.
    Both act on **one node at a time** and both reach only
    `agents/<agent-id>/` — the agent's own folder, never the folder itself,
    never a teammate's, never shared guidance. `workspace_delete` carries the
    same required `expected_updated_at` token as `workspace_write` and refuses a
    folder that still holds anything, so a subtree is removed as N deliberate,
    individually-parked calls rather than one. `workspace_rename` carries no
    token, because it destroys nothing: body, id and both authorship stamps
    survive it. Read the confinement as a division of labour rather than as a
    security boundary — the same grant already confers *unconfined* overwrite,
    which reaches further than own-folder lifecycle does. Renaming or deleting
    anything elsewhere in the tree stays operator-only.

    Agent writes are broad: an agent may create or edit ordinary shared content
    anywhere in its company's tree. The reserved lowercase `secrets/` subtree
    is the exception: boot creates it with an explanatory `README.md`, and
    agent workspace list/read/search/write/create tools omit or refuse the
    entire subtree while operator workspace APIs retain full access. This is a
    model-visibility boundary, not the application credential store; provider
    and tool credentials still belong in Connections/inference settings.
    Confining other creation while leaving overwrite free would
    protect nothing. What keeps the tree navigable instead is steering plus
    attribution — the persona brief names the agent's own reserved folder
    `agents/<agent-id>/` (minted the first time that agent puts something in it;
    boot scaffolds the empty `agents/` root plus `secrets/README.md`, and since
    issue #645 `desks/` is minted on first use rather than scaffolded) as the default
    home for what it produces and marks shared
    guidance as something to edit only on purpose, and every node records who
    created it and who last wrote it (issue #326), which the console shows. Both
    sides are capped at the agent harness's own per-tool-result byte budget
    minus the framing a read wraps a body in (issue #417) — 12 KiB today,
    derived rather than chosen so a full read always survives the harness cut
    and the write gate is measured against the bytes the model actually
    received. A larger note is agent-read-only: the agent sees a truncated body,
    and a write against it is refused rather than discarding the part it could
    not see, so only an operator can edit it in the console. **Operator edits
    are not capped by this** — the console and the REST handlers write through
    the `WorkspaceStore` port directly and never enter the agent tool path. Under
    `supervised` no longer adds an approval prompt; under `readonly` a write is
    denied — reads stay available in every mode. The namespace is **not**
    gateable by `[plan].token_budgets`: reads
    cost nothing and shedding them would only make agents guess at company
    standards. The tools hit the store per call, so an operator's console edit
    is visible to the next turn with no restart.
- **`[[schedule]]`** entries become `ScheduleFired` events; cron syntax is
  standard 5-field, interpreted in UTC. A saved *workflow* schedules itself
  separately, with the same dialect: its `trigger` node carries a `schedule`
  cron that the workflow scheduler fires (issue #169). A manifest schedule
  drives a company cycle; a trigger schedule drives one workflow run.
- **`[workflows]`** enables saved graphs and bounds how many may run at once.
  `enabled` lists the `workflows/<id>.toml` ids to turn on. `max_in_flight_runs`
  (issue #401) is the company's concurrent-run ceiling — default **8**,
  validated **>= 1** (a `0` would refuse every run and is rejected at load). It
  applies to *every* entry point that starts a run (the manual run route, the
  cron scheduler, an approved gate's continuation, and the orchestrator's
  `run_workflow` tool), enforced at one choke point. The default sits above 1
  deliberately: a running workflow's agent node can call `run_workflow` while
  the parent run still holds a slot, so a ceiling of 1 would refuse legitimate
  nesting. A run over the ceiling is **refused, never queued** — the run route
  answers `429` (see `api.md`), a scheduled fire is skipped for that minute, and
  the orchestrator tool tells the agent to wait. A slot frees the instant a run
  settles.
