// Prosumer-facing language. The spec's glossary is normative: product/UI text
// never exposes runtime internals ("agent graph", "tier", "dispatch", "cycle",
// "checkpoint", "A2A"). Everything a person sees goes through this layer.

import type { TaskApprovalStatus } from "../api/tasks";
import type { ApprovalSummary, FeedbackCategory, StandingGrant } from "../api/types";

/**
 * A company's lifecycle state, in plain language, with a status tone.
 *
 * `emergencyPaused` (issue #86) outranks every lifecycle value, because the
 * kill switch is orthogonal to lifecycle: a stopped company still reports
 * `running`, so a caller that passed only `state` would render "Live" over a
 * company that is refusing to do anything. Routing it through here rather than
 * through each indicator means every existing surface — the sidebar row, the
 * switcher, the picker — reports it without its own branch.
 */
export function lifecycle(
  state: string,
  emergencyPaused = false,
): { label: string; tone: "live" | "idle" | "stopped" } {
  if (emergencyPaused) return { label: "Emergency stop", tone: "stopped" };
  switch (state) {
    case "running":
      return { label: "Live", tone: "live" };
    case "onboarding":
      return { label: "Setting up", tone: "idle" };
    case "drafted":
      return { label: "Draft", tone: "idle" };
    case "paused":
      return { label: "Paused", tone: "idle" };
    case "suspended":
      return { label: "Suspended", tone: "stopped" };
    case "archived":
      return { label: "Archived", tone: "stopped" };
    default:
      return { label: titleCase(state), tone: "idle" };
  }
}

/**
 * What became of one of a task's approvals, in plain language.
 *
 * `expired` is the reason this exists (#971). It has been in
 * {@link TaskApprovalStatus} since #333 and nothing could ever produce it,
 * because nothing swept approvals for a company without a manifest schedule —
 * so no surface ever needed words for it and none had any. Now that requests
 * age out for every company, the state is reachable, and a surface reaching it
 * without a phrase here would print the raw identifier at an operator: the one
 * thing this module exists to prevent.
 *
 * The wording carries the distinction that matters. "Declined" and "Expired"
 * are both a no, and the whole point of #971's honesty work is that an
 * operator can tell the no they made from the one the deadline made — the same
 * distinction the event stream's `automatic` flag carries.
 *
 * Exhaustive over the union rather than a table with a fallback: the union is
 * closed and small, so a member added later should fail the build here rather
 * than quietly render as a runtime identifier.
 */
export function approvalStatusLabel(status: TaskApprovalStatus): string {
  switch (status) {
    case "pending":
      return "Waiting on you";
    case "approved":
      return "Approved";
    case "denied":
      return "Declined";
    case "expired":
      return "Expired — nobody decided in time";
  }
}

/**
 * A parked effect kind → what the company wants to do, in plain language.
 *
 * Deliberately un-annotated so its keys stay literal: {@link EFFECT_DONE_LABELS}
 * is checked against them, which is what stops the two tables drifting.
 */
const EFFECT_LABELS = {
  "payment.send": "Send a payment",
  "subscription.start": "Start a subscription",
  "email.send": "Send an email",
  "dm.external": "Message someone new",
  "filing.submit": "Submit a filing",
  "contract.accept": "Accept a contract",
  "external.publish": "Publish something publicly",
  "website.deploy": "Deploy a website change",
  "handle.register": "Claim a public handle",
  "handle.renew": "Renew a public handle",
  "key.rotate": "Rotate its security key",
  // A tool call the company parked mid-conversation. The effect kind is the
  // tool's own name, which is a runtime internal — the glossary rule above says
  // an operator never sees one, so the gated tools get plain-language labels
  // rather than the title-cased fallback ("Composio Execute").
  composio_authorize: "Connect one of its accounts",
  composio_execute: "Act in one of its connected accounts",
  mcp_registry_tool_call: "Use a connected tool",
  media_generate_image: "Generate an image",
  media_generate_video: "Generate a video",
  // A workflow run that paused on a step marked "needs approval" (#395). The
  // card is journal-driven like every other, so it renders through the existing
  // projection — but without a glossary entry it fell through to "Do something
  // that needs your sign-off", which tells an operator nothing about what they
  // are about to restart. The payload names the workflow and the step.
  "workflow.approve": "Continue a paused workflow",
};

export function effectAction(kind: string): string {
  return labelFor(EFFECT_LABELS, kind) ?? titleCase(kind.replace(/[._]/g, " "));
}

/**
 * The same effects in the past tense (#351) — what a task ALREADY did.
 *
 * A separate table rather than a suffix rule on {@link EFFECT_LABELS}: every
 * entry there is phrased as a request the company is making ("Send a payment"),
 * and a retry warning has to state a fact ("Sent a payment"). Bending one into
 * the other mechanically produces sentences no editor would sign off on.
 *
 * `satisfies` makes the mirror exhaustive **both ways** — a kind added to
 * {@link EFFECT_LABELS} and not here is a typecheck failure rather than a
 * silently title-cased effect key, and a key here that is not a real effect kind
 * is one too. That is also why entries stay for kinds no path currently reaches
 * the retry dialog with: the table's contract is "every kind the glossary
 * names", not "every kind observed in a journal", and pruning to the latter
 * would break the moment a classification changed.
 */
const EFFECT_DONE_LABELS = {
  "payment.send": "Sent a payment",
  "subscription.start": "Started a subscription",
  "email.send": "Sent an email",
  "dm.external": "Messaged someone new",
  "filing.submit": "Submitted a filing",
  "contract.accept": "Accepted a contract",
  "external.publish": "Published something publicly",
  "website.deploy": "Deployed a website change",
  "handle.register": "Claimed a public handle",
  "handle.renew": "Renewed a public handle",
  "key.rotate": "Rotated its security key",
  composio_authorize: "Connected one of its accounts",
  composio_execute: "Acted in one of its connected accounts",
  mcp_registry_tool_call: "Used a connected tool",
  media_generate_image: "Generated an image",
  media_generate_video: "Generated a video",
  "workflow.approve": "Continued a paused workflow",
} satisfies Record<keyof typeof EFFECT_LABELS, string>;

/**
 * What a company already did, in plain language, with the amount when there is
 * one: "Sent a payment of $2,400.00" (#351).
 *
 * An unmapped kind falls back to a generic sentence rather than to a
 * title-cased key. The kind of an effect projected from a tool call *is* the
 * tool's own name, so title-casing it puts a runtime internal
 * ("Slack Post Message") in front of an operator — which the glossary rule at
 * the top of this file forbids, and which reads as a system leak in exactly the
 * dialog that has to be trusted. Saying less is the better failure.
 */
export function effectDone(kind: string, amountUsd?: number | null): string {
  const action = labelFor(EFFECT_DONE_LABELS, kind);
  if (action) return amountUsd != null ? `${action} of ${money(amountUsd)}` : action;
  return amountUsd != null
    ? `Did something that cannot be undone, involving ${money(amountUsd)}`
    : "Did something that cannot be undone";
}

/**
 * Gated **tool** names → what using that tool means, in plain language (#372).
 *
 * Kept apart from {@link EFFECT_LABELS} on purpose. That table's entries are
 * business effects, which are self-describing at kind level: `payment.send`
 * without its amount still tells an operator what is about to happen. A tool
 * name does not — `shell` without its command is meaningless — so these labels
 * only ever appear *above* the payload block that says what the tool will do.
 * Keeping them out of `EFFECT_LABELS` also leaves the
 * `EFFECT_LABELS`/`EFFECT_DONE_LABELS` `satisfies` mirror undisturbed.
 */
const TOOL_LABELS: Readonly<Record<string, string>> = {
  shell: "Run a terminal command",
  glob: "Search files in its workspace",
  // Issue #374 added a second reader of these labels — the Standing permissions
  // list — where the payload block that used to disambiguate an unlabelled tool
  // does not exist. Two permissions both reading "Use one of its tools" would be
  // indistinguishable, so the tools that can actually hold one (the catch-all
  // `Other` group) need real words rather than the generic fallback.
  // Billing (issues #788, #789). Only the two that park need words here; the
  // read tools never reach an approval card.
  chargebee_send_invoice: "Send an invoice to a customer",
  chargebee_create_customer: "Add a customer to Chargebee",
  workspace_write: "Edit a note in its workspace",
  workspace_read: "Read a note in its workspace",
  workspace_list: "List its workspace notes",
  // Every workspace mutation parks per call, so each of these reaches an
  // operator on the approval card and again in the Standing permissions list.
  // `workspace_create` has been missing since issue #551 and was showing as the
  // generic "Use one of its tools"; the lifecycle pair (#671) would have
  // arrived with the same gap. "its own folder" is load-bearing on the last
  // two — the scope is the whole reason an operator can wave them through.
  workspace_create: "Add a note to its workspace",
  workspace_rename: "Rename a note in its own folder",
  workspace_delete: "Delete a note from its own folder",
  // Issue #245's pair. Both park per call, so each reaches an operator on the
  // approval card and again in the Standing permissions list — and both names
  // read like reads, which is exactly when a label has to say what is actually
  // happening. "One of the company's repositories" is load-bearing: it tells
  // the operator the reach is what they bound, not the whole of GitHub.
  repo_checkout: "Check out one of the company's repositories",
  repo_pr: "Fetch a pull request from one of the company's repositories",
  memory_store: "Save something to its memory",
  memory_recall: "Look something up in its memory",
  web_fetch: "Fetch a web page",
  query_company: "Look up something about the company",
  // Issue #701 — the rest of the gated surface, found the way #671 was: by
  // cross-checking every `Reach::Consequence` declaration in
  // `src/policy/consequence.rs` against the keys of both tables here, rather
  // than by anyone noticing a card. `every_consequence_tool_has_a_console_label`
  // in that file is now what keeps the two in step.
  //
  // All of these park under their own raw tool name. That was the issue's open
  // question and it is pinned as a test (`parked_kind_is_the_tool_name` in
  // `src/harness/policy.rs`) rather than left to prose, because two of them
  // invite the opposite guess: `publish_artifact` does NOT park as
  // `external.publish` (a native workflow-gate class the tool never builds), and
  // `run_workflow` does NOT park as `workflow.approve` (that is a workflow
  // resuming mid-run, #395 — a different event from an agent asking to start
  // one). A label filed under a kind nothing parks with is a label nobody sees.
  //
  // They belong here rather than in EFFECT_LABELS for the reason stated above:
  // none is self-describing without the payload block underneath it — `curl`
  // without its address says nothing — and EFFECT_LABELS' `satisfies` mirror
  // would additionally demand a past-tense twin for a retry dialog these kinds
  // never reach as native effects.
  curl: "Download a file from the internet",
  http_request: "Make a request to a web address",
  // Three network tools, three sentences, and the differences are read off what
  // the tools actually take rather than off their names. `curl` is not the
  // arbitrary-method one its name suggests — it accepts a `url` and streams the
  // body to a file in the workspace — while `http_request` carries any of GET,
  // POST, PUT, DELETE, PATCH, HEAD, OPTIONS with headers and a body, and
  // `web_fetch` above returns a page inline. Two labels an operator cannot tell
  // apart are the #374 defect in a different costume.
  git_operations: "Run a git command in its workspace",
  read_workspace_state: "Check its workspace's git status",
  // Both name git, which reads like a runtime internal and is not one here: it
  // is what these tools literally run, the console already says so to an
  // operator (`denial_reason` in `consequence.rs` explains a readonly refusal in
  // exactly those words), and the alternative — "check its version control" over
  // a payload block reading `git log` — is the vaguer of the two, not the
  // plainer. `read_workspace_state` is gated *because* it shells out to git in a
  // directory the agent can write git's own config into (#459); the operator is
  // told what it does, and the reason it is gated stays the classifier's
  // business.
  mcp_call_tool: "Use a tool on a connected server",
  // Distinct wording from `mcp_registry_tool_call`'s "Use a connected tool"
  // below on purpose — the two are separate gates and an operator seeing both in
  // the Standing permissions list has to be able to tell which is which.
  publish_artifact: "Publish a file it produced",
  // Not "…publicly", though the declaration's own comment calls publishing
  // "externally visible": the tool's description is the narrower and more
  // careful claim — an agent's sandbox is private, and publishing is the only
  // thing that hands a finished file over. A card that says "publicly" over a
  // hand-off to the operator is the misleading-label failure this issue refused
  // to risk, so the label states what is true under either reading.
  run_workflow: "Run one of its saved workflows",
  // Issue #661 (M7). Only the delete of the three parks — `read_workflow` and
  // `update_workflow` are `Reach::Nothing` — so only the delete needs words
  // here, and "permanently" is the load-bearing one: an update keeps the prior
  // version in the workflow's history, while this takes that history with it.
  delete_workflow: "Permanently delete one of its saved workflows",
  // The four tools an operator may grant standing on (#444). They are not in the
  // catch-all `Other` group by accident — they are the low-consequence writes
  // the standing-grant feature exists to apply to, which means they are the
  // entries most likely to sit next to each other in the #374 Standing
  // permissions list, where no payload block exists to tell them apart. Four
  // rows all reading "Use one of its tools" is precisely the state that list was
  // added to end.
  file_write: "Write a file in its workspace",
  edit: "Edit a file in its workspace",
  // `apply_patch` is a batch of exact-string edits applied atomically across one
  // or more files — "a patch" in the diff sense is what it is *not*, and the
  // count is the whole difference between it and `edit` above.
  apply_patch: "Edit several files in its workspace at once",
  csv_export: "Save data as a spreadsheet file in its workspace",
  // `mcp_registry_tool_call` is deliberately absent: EFFECT_LABELS already
  // names it and is consulted first, so an entry here would be unreachable.
};

/**
 * What an approval is asking for, in plain language (#372).
 *
 * Resolution order, and why the last two rungs differ:
 *
 * 1. the effect glossary — a business effect, phrased as it always was;
 * 2. {@link TOOL_LABELS} — a gated tool we have words for;
 * 3. an unmapped kind **with** an `agent` — it came from a tool call, so we can
 *    at least say a teammate wants to use a tool;
 * 4. an unmapped kind with no agent — a native effect nobody has named.
 *
 * The title-cased fallback in {@link effectAction} is never reached from here.
 * That fallback is what put "Glob" and "Shell" — raw runtime identifiers — in
 * front of an operator, which is the bug #372 opens with and which the glossary
 * rule at the top of this file forbids. `effectAction` itself is deliberately
 * left alone so no other surface shifts under this change.
 */
export function approvalAction(a: ApprovalSummary): string {
  // Issue #846: a paused workflow gate is named by the CALL it is stopping, not
  // by the mechanism that stopped it. "Continue a paused workflow" is true of
  // every one of these cards and therefore tells an operator nothing — it is
  // #372's "Glob" complaint, one surface over: the chat path was fixed by #375
  // and this path was not, because its label came from `EFFECT_LABELS` (which
  // keys on the effect kind, and the kind here is always `workflow.approve`)
  // rather than from the tool.
  //
  // The tool is on the wire whenever the host could classify the node's call.
  // When it could not — an authored gate on a step that calls nothing — the
  // glossary entry below is still the honest answer, so this promotes and never
  // hides.
  const workflowTool = workflowGateTool(a);
  if (workflowTool) return toolAction(workflowTool);
  return (
    labelFor(EFFECT_LABELS, a.kind) ??
    labelFor(TOOL_LABELS, a.kind) ??
    (a.agent ? "Use one of its tools" : "Do something that needs your sign-off")
  );
}

/** The effect kind a paused workflow gate parks as — mirrors
 * `WORKFLOW_APPROVE_KIND` in `src/runtime/workflow_resume.rs`. */
const WORKFLOW_APPROVE_KIND = "workflow.approve";

/**
 * The tool a paused workflow gate is stopping, when the host named one
 * (issue #846) — `null` for every other kind, and for a gate whose node makes
 * no classifiable call.
 *
 * Reads `payload.tool`, which the host writes for a policy-raised gate (#460)
 * and, since #846, for an authored one too. Absence is meaningful and is
 * preserved as such: an **older host** omits the key entirely, and this then
 * returns `null` so the card renders exactly the pre-#846 line rather than an
 * empty label.
 */
function workflowGateTool(a: ApprovalSummary): string | null {
  if (a.kind !== WORKFLOW_APPROVE_KIND) return null;
  const payload = a.payload;
  if (payload == null || typeof payload !== "object" || Array.isArray(payload)) return null;
  const tool = (payload as Record<string, unknown>).tool;
  return typeof tool === "string" && tool !== "" ? tool : null;
}

/**
 * What a tool does, in plain language, from its identifier alone (#374).
 *
 * The Standing permissions list has no approval to hand to
 * {@link approvalAction} — the card it came from was resolved and is gone — but
 * the glossary rule at the top of this file still applies: an operator must
 * never be shown `workspace_write` and asked to reason about it. Same first two
 * rungs as {@link approvalAction}, with a fallback that names a tool without
 * pretending to know which one.
 */
export function toolAction(kind: string): string {
  return labelFor(EFFECT_LABELS, kind) ?? labelFor(TOOL_LABELS, kind) ?? "Use one of its tools";
}

/**
 * What one standing permission actually covers, in one line (#457).
 *
 * {@link toolAction} alone answers "which tool", which is the whole sentence for
 * `file_write` and only half of it for `composio_execute`: that name carries
 * every action of every connected provider, so a row reading "Act in one of its
 * connected accounts" leaves an operator unable to tell a permission over their
 * code host from one over their mailbox — and a permission you cannot read is
 * one you cannot decide to revoke. When the host names a scope, the row names
 * what it narrows to.
 *
 * Composed as a suffix rather than a second phrasing table: the tool's own words
 * stay in {@link toolAction}, so there is nothing here to drift out of step
 * with it.
 *
 * Lives here rather than in the view because it now has a branch worth naming
 * ({@link scopeLabel}), and the branch is the whole of issue #785.
 */
export function grantHeadline(g: StandingGrant): string {
  const action = toolAction(g.tool);
  return g.scope ? `${action} — ${scopeLabel(g.scope)} only` : action;
}

/**
 * A grant's scope as a person would read it (#785).
 *
 * `StandingGrant.scope` is one string carrying **two different kinds** of value,
 * minted by two arms of the same host function (`standing_scope_of`):
 *
 * * a **Composio toolkit** identifier — `microsoft_teams` — which is a slug and
 *   has to be spelled out before an operator can read it;
 * * a **URL origin** — `https://docs.rs` — added for `web_fetch` by #673/#739,
 *   which is already exactly what the operator approved and must survive
 *   untouched.
 *
 * Issue #785 was the second kind going through the first kind's speller:
 * `https://docs.rs` has no `_`, so it stayed one "word" and came out as
 * `Https://docs.rs`. A scheme is not a proper noun, and an operator reading a
 * row *in order to decide whether to revoke it* should not have to wonder
 * whether the display is lying about the rest of the string too.
 *
 * The kinds are told apart here, on the value, rather than by a discriminator
 * on the wire. `scope` is the enforcement key — the host matches a live call
 * against it with exact string equality (`StandingGrant::admits_scope`) and it
 * is replayed from the journal — so retyping it is a persisted-format change to
 * a security-relevant comparison, not a label fix. What that suggestion is
 * really worth is naming the two kinds at the type, which the doc comments on
 * both sides now do. If a third kind ever arrives, this function is the one
 * place that has to learn about it.
 */
function scopeLabel(scope: string): string {
  return scope.includes("://") ? scope : toolkitLabel(scope);
}

/**
 * A toolkit identifier as a person would write it: `microsoft_teams` →
 * "Microsoft Teams".
 *
 * Mechanical on purpose. A lookup table of pretty provider names would render
 * the ~30 toolkits it knew and the raw slug for everything else, and the ones it
 * did not know would be exactly the ones added most recently.
 *
 * Toolkit slugs only — see {@link scopeLabel} for why a URL origin must never
 * reach this.
 */
function toolkitLabel(scope: string): string {
  return scope
    .split("_")
    .filter(Boolean)
    .map((word) => word.charAt(0).toUpperCase() + word.slice(1))
    .join(" ");
}

/** A one-line, human summary of what needs approval. */
export function approvalSummary(a: ApprovalSummary): string {
  const action = approvalAction(a);
  if (a.amount_usd != null) return `${action} — ${money(a.amount_usd)}`;
  return action;
}

/** One line of an approval's payload preview: a label and its value. */
export interface PayloadLine {
  label: string;
  value: string;
}

/**
 * The payload the host sent, as lines an operator can read (#372).
 *
 * The values are already redacted and bounded host-side, so this function's
 * only job is presentation. Two shapes:
 *
 * * a kind we know the argument names of (`shell`, `glob`) gets its meaningful
 *   arguments in a fixed, readable order — the command first, because that is
 *   the thing being consented to;
 * * anything else falls back to `key: value` over the payload's own top-level
 *   entries, which is still concrete and still safe.
 *
 * Nested objects and arrays are re-serialized as compact JSON rather than
 * dropped: an operator approving a structured call needs to see the structure.
 * Returns `[]` when there is nothing to show, which is also what an **old
 * host** produces (it omits `payload` entirely) — callers render the pre-#372
 * one-line card in that case.
 */
export function payloadLines(a: ApprovalSummary): PayloadLine[] {
  const payload = a.payload;
  if (payload == null) return [];
  if (typeof payload !== "object") return [{ label: "value", value: renderValue(payload) }];
  if (Array.isArray(payload)) return [{ label: "items", value: renderValue(payload) }];

  const entries =
    a.kind === WORKFLOW_APPROVE_KIND
      ? workflowGateEntries(payload as Record<string, unknown>)
      : Object.entries(payload as Record<string, unknown>);
  if (entries.length === 0) return [];

  // Preferred ordering for the kinds whose argument names we know. Unlisted
  // arguments still follow — this promotes, it never hides.
  const preferred = PAYLOAD_KEY_ORDER[a.kind] ?? [];
  const rank = (key: string) => {
    const i = preferred.indexOf(key);
    return i === -1 ? preferred.length : i;
  };
  return entries
    .filter(([, value]) => value != null && value !== "")
    .sort(([a1], [b1]) => rank(a1) - rank(b1))
    .map(([label, value]) => ({ label, value: renderValue(value) }));
}

/**
 * The payload of a paused workflow gate, as the lines an operator needs
 * (issue #846).
 *
 * A `workflow.approve` payload is not a tool call's arguments — it is the host's
 * **resume record**, and every other card's rule ("show the payload, it is what
 * you are consenting to") produces exactly the wrong thing here. What an
 * operator saw was `input: {"items":[{"json":{}}],"port":null}`: the engine's
 * seed payload, verbatim, as the sole description of a decision.
 *
 * So this promotes the call's own arguments to the top level, where every other
 * card carries them, and drops the machinery. The dropped keys are dropped for
 * stated reasons, not for tidiness:
 *
 * * `input` — the resume payload. It is engine seed data, it is the thing that
 *   read as a description and was not one, and it also carries an `approvals`
 *   list that accumulates down a lineage: an operator seeing
 *   `{"approvals":["fetch_bbc","fetch_espn"]}` reads it as a card covering all
 *   of them, which it is not. (Consolidating several gates into one card is
 *   real and is issue #842 — not something to imply by accident here.)
 * * `delivered`, `performed` — the #438/#846 ledgers. They say what will NOT
 *   happen again, which is a property of the mechanism and is already stated in
 *   prose by `note`.
 * * `content` — rendered in full by its own block (`WorkflowContentReview`,
 *   #596); repeating it as one clamped JSON line would be strictly worse.
 * * `note` — prose, rendered as prose elsewhere on the card, not as a
 *   `key: value` pair in a monospace block.
 *
 * Everything else survives, including any key a **newer host** adds that this
 * console has never heard of. Dropping unknown keys would make an old console
 * silently hide new information, which is the failure mode this whole file is
 * written against; the denylist is closed and the allowlist is not.
 */
function workflowGateEntries(payload: Record<string, unknown>): [string, unknown][] {
  const args = payload.args;
  const argEntries: [string, unknown][] =
    args != null && typeof args === "object" && !Array.isArray(args)
      ? Object.entries(args as Record<string, unknown>)
      : [];
  const rest = Object.entries(payload).filter(([key]) => !WORKFLOW_GATE_HIDDEN.has(key));
  // The call first, then where and what stopped it. An operator decides on the
  // call; the node id is how they find it afterwards. This order survives
  // `payloadLines`' sort because no `PAYLOAD_KEY_ORDER` entry exists for this
  // kind — every rank is equal, and the sort is stable.
  return [...argEntries, ...rest];
}

/** Payload keys of a `workflow.approve` card that are machinery, not the
 * decision — see {@link workflowGateEntries} for why each one is here. */
const WORKFLOW_GATE_HIDDEN: ReadonlySet<string> = new Set([
  "input",
  "delivered",
  "performed",
  "content",
  "note",
  // Unwrapped one level above, so keeping it would print the same arguments
  // twice — once readably and once as a JSON blob.
  "args",
]);

/** Which arguments lead the preview, per tool. Presentation only. */
const PAYLOAD_KEY_ORDER: Readonly<Record<string, string[]>> = {
  shell: ["command", "cwd", "timeout"],
  glob: ["pattern", "path"],
  // The #701 labels lean on this block harder than the two above do. "Download a
  // file from the internet" is only half a question until the operator can see
  // *which* address, and both network payloads carry headers and a body that
  // will happily push the url off the first line. Same for the git operation,
  // which is the entire difference between reading a log and committing.
  //
  // Key names are the tools' own, not guesses: `curl` takes `url`/`dest_path`
  // and no method (it streams a download to disk), `http_request` takes
  // `url`/`method`, and `git_operations` takes `operation` — an enum, not a
  // command line. Nothing is hidden here; this promotes, and every unlisted
  // argument still follows.
  curl: ["url", "dest_path"],
  http_request: ["url", "method"],
  git_operations: ["operation"],
};

function renderValue(value: unknown): string {
  if (typeof value === "string") return value;
  return JSON.stringify(value) ?? String(value);
}

export function money(usd: number): string {
  return usd.toLocaleString(undefined, { style: "currency", currency: "USD" });
}

/** Feedback categories, phrased the way an operator would think about them. */
export const FEEDBACK_CATEGORIES: { value: FeedbackCategory; label: string }[] = [
  { value: "wrong-output", label: "This was wrong" },
  { value: "bug", label: "Something broke" },
  { value: "missing-capability", label: "It can't do something I need" },
  { value: "approval-friction", label: "It asks too much / too little" },
  { value: "template-gap", label: "The team is missing a role" },
  { value: "docs", label: "The docs are unclear" },
];

/** A short relative time like "2m ago", "3h ago", "just now". */
/**
 * Does approving this put something outside the company (#1024)?
 *
 * Reads the host's own `group`, never the effect `kind`. For a harness tool call
 * `kind` IS the tool name — `composio_execute`, not `email.send` — so a predicate
 * keyed on `kind` would match the native effects the icon table knows and miss
 * the composio `GMAIL_SEND_EMAIL` send this was reported for. `group` is derived
 * host-side from the tool *and its arguments*, the only place that is known.
 *
 * `other` is the catch-all internal bucket — the same line the host draws for
 * `broadly_grantable`. An absent `group` is an older host and reads as internal,
 * so the card renders exactly as it did before rather than labelling something
 * this console cannot classify.
 */
export function leavesTheCompany(a: ApprovalSummary): boolean {
  return a.group != null && a.group !== "other";
}

/**
 * How old the parked payload is, and whether to say so loudly (#1024).
 *
 * `at_millis` is stamped when the effect is parked, in the same turn that
 * composed its arguments — so it dates the payload, not the queue, and
 * "Composed" is the honest word. "Parked" would be closer to the field name and
 * would reintroduce the exact reading this fixes: parking is a queue event, and
 * a queue reading is what let a five-day-old digest look routine.
 *
 * Labelled only for effects that leave the company. Elsewhere the age genuinely
 * IS queue latency, and labelling it everywhere would spend the emphasis where
 * it does not matter and dilute it where it does.
 */
export function payloadAge(a: ApprovalSummary, now: number): { text: string; emphasise: boolean } {
  const age = timeAgo(a.at_millis, now);
  return leavesTheCompany(a)
    ? { text: `Composed ${age}`, emphasise: true }
    : { text: age, emphasise: false };
}

export function timeAgo(atMillis: number, now: number): string {
  const secs = Math.max(0, Math.floor((now - atMillis) / 1000));
  if (secs < 45) return "just now";
  const mins = Math.floor(secs / 60);
  if (mins < 60) return `${mins}m ago`;
  const hours = Math.floor(mins / 60);
  if (hours < 24) return `${hours}h ago`;
  const days = Math.floor(hours / 24);
  return `${days}d ago`;
}

/**
 * "in 42m" / "in 6h" / "in 3d" — how long something has left before a deadline.
 *
 * The counterpart to {@link timeAgo}, and it lives beside it for the reason
 * this module exists: two places rendering a deadline in two vocabularies is
 * how an operator ends up comparing "in 6h" on one row with "6 hours" on the
 * next and wondering whether they mean the same thing. It was written for the
 * standing-permission rows (#374) and is shared with the approval card's
 * deadline (#971) rather than copied, so the buckets cannot drift apart.
 *
 * Clamped at zero: a deadline already passed reads "in 0m", never a negative.
 * The caller decides what a passed deadline should say — the grants list, for
 * instance, renders "expired" instead of calling this at all — because "what
 * happens after the deadline" differs by surface and is not a formatting
 * question.
 */
export function untilLabel(atMillis: number, now: number): string {
  const mins = Math.max(0, Math.round((atMillis - now) / 60_000));
  if (mins < 60) return `in ${mins}m`;
  const hours = Math.round(mins / 60);
  if (hours < 24) return `in ${hours}h`;
  return `in ${Math.round(hours / 24)}d`;
}

/**
 * Indexes a label table by a kind that arrives at runtime and may not be in it.
 *
 * The tables keep literal key types so they can be checked against each other;
 * this is the one place that widens them back to a lookup, so the widening is
 * deliberate rather than an annotation that quietly disables the check.
 */
function labelFor(table: Readonly<Record<string, string>>, kind: string): string | undefined {
  return table[kind];
}

function titleCase(s: string): string {
  return s.replace(/\w\S*/g, (w) => w.charAt(0).toUpperCase() + w.slice(1).toLowerCase());
}
