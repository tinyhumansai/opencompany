// One agent, read and edited (issue #264). The pure half of the detail view:
// the field definitions both the create dialog and the edit form render from,
// and the three derivations that are easy to get quietly wrong.

import type { AgentDetailDto, AgentToolsDto, EditAgentInput, HarnessDto } from "@/api/types";

/** The fields that describe an agent, in the order both forms show them. */
export type AgentFieldKey = "name" | "role" | "description" | "instructions";

/**
 * Everything the host may report as editable — the form fields above, plus
 * `tools`.
 *
 * `tools` is deliberately not an [`AgentFieldKey`]: it is a list of globs
 * rather than a line of prose, it is admin-only where the others are
 * member-open, and it has its own card. Keeping it out of `AGENT_FIELDS` is
 * what stops the shared draft form from rendering it as a text field and
 * echoing it back on every unrelated save.
 */
export type AgentEditableKey = AgentFieldKey | "tools";

export interface AgentFieldSpec {
  key: AgentFieldKey;
  label: string;
  placeholder: string;
  /** `prose` renders a textarea; `line` renders a single-line input. */
  kind: "line" | "prose";
  /**
   * How many rows a `prose` field's textarea shows. Instructions is the whole
   * persona an agent runs on — longer than a one-line "what they do" — so it
   * asks for more room. Ignored for a `line` field.
   */
  rows?: number;
  /**
   * Whether a save is refused while this field is blank — the rule
   * [`draftIsValid`] enforces, stated on the field itself so the form can SHOW
   * it (issue #1776).
   *
   * It went unsaid for too long, and the cost was specific. A manifest
   * teammate carries no name of its own (it is addressed by its role, and every
   * card renders `name || role`), so the edit form opens with Role filled, Name
   * blank, and Save already dead — with nothing on screen saying which field is
   * responsible. The requirement is right; being invisible was not.
   */
  required?: boolean;
}

/**
 * The single definition of an agent's authored fields.
 *
 * Shared deliberately by "Add teammate" and the detail view's edit form: the
 * two collect the same three things, and before this they would have collected
 * them under two sets of labels and placeholders that drifted apart. The host
 * accepts exactly these keys in a `PATCH`, so the list is also the client half
 * of that contract.
 */
export const AGENT_FIELDS: AgentFieldSpec[] = [
  { key: "name", label: "Name", placeholder: "e.g. Nova", kind: "line", required: true },
  {
    key: "role",
    label: "Role",
    placeholder: "e.g. Growth Marketer",
    kind: "line",
    required: true,
  },
  {
    key: "description",
    label: "What they do",
    placeholder: "e.g. Runs paid acquisition and reports on ROAS.",
    kind: "prose",
  },
  {
    key: "instructions",
    label: "Instructions",
    placeholder:
      "e.g. Always confirm the budget before launching a campaign. Report ROAS weekly and flag anything under 2x.",
    kind: "prose",
    // The persona the agent runs on, appended verbatim to its system prompt —
    // longer than the one-line "what they do", so it gets a taller box.
    rows: 8,
  },
];

/** The three authored values, as a form holds them. */
export type AgentDraft = Record<AgentFieldKey, string>;

/** A blank draft, for the create form and for a detail view that has not loaded. */
export function emptyDraft(): AgentDraft {
  return { name: "", role: "", description: "", instructions: "" };
}

/** The draft a detail response starts an edit from. */
export function draftFrom(detail: AgentDetailDto): AgentDraft {
  return {
    // A manifest teammate carries no name of its own and is shown by its role,
    // so the form starts from the same thing the card does.
    name: detail.name ?? "",
    role: detail.role,
    description: detail.description ?? "",
    // The **effective** instructions — the override when one is set, else the
    // blueprint seed — so an edit starts from what the agent actually runs on.
    instructions: detail.instructions ?? "",
  };
}

/**
 * Whether the host will accept an edit to this field.
 *
 * Read from the host's own `editable` list rather than inferred from `source`.
 * The two agree today, and the moment they stop agreeing the host is right:
 * duplicating the rule here is how a console starts offering a field that saves
 * with a 409.
 */
export function isEditable(detail: AgentDetailDto, key: AgentEditableKey): boolean {
  return detail.editable.includes(key);
}

/**
 * The `PATCH` body for a draft, or `null` when nothing changed.
 *
 * Two rules, and both matter:
 *
 * - **Only changed, editable fields are sent.** An unchanged field is left out
 *   entirely, so a form that renders a read-only field cannot echo it back and
 *   have the host refuse the whole save.
 * - **A cleared description is `null`, not `""`.** `undefined` means "leave the
 *   instructions alone" on the wire; sending `undefined` for a field the
 *   operator deliberately emptied would silently keep the old text, and the
 *   operator would watch their deletion come back.
 */
export function agentEdits(detail: AgentDetailDto, draft: AgentDraft): EditAgentInput | null {
  const current = draftFrom(detail);
  const edits: EditAgentInput = {};
  let changed = false;

  for (const field of AGENT_FIELDS) {
    if (!isEditable(detail, field.key)) continue;
    const next = draft[field.key].trim();
    if (next === current[field.key].trim()) continue;
    changed = true;
    if (field.key === "description") {
      edits.description = next === "" ? null : next;
    } else if (field.key === "instructions") {
      // Same three-state as description: an emptied field is `null`, which on
      // the host clears the override and resets the teammate to its blueprint —
      // not `""`, which would try to store an empty persona.
      edits.instructions = next === "" ? null : next;
    } else if (field.key === "name") {
      edits.name = next;
    } else {
      edits.role = next;
    }
  }

  return changed ? edits : null;
}

/** Whether a draft could be saved at all: name and role are required. */
export function draftIsValid(detail: AgentDetailDto, draft: AgentDraft): boolean {
  return missingRequired(draft, (key) => isEditable(detail, key)).length === 0;
}

/**
 * The required fields this draft leaves blank, in form order (issue #1776).
 *
 * The same predicate [`draftIsValid`] answers as a boolean, but naming the
 * fields instead of hiding them behind one — so a form can mark the offending
 * box and say why its Save is dead, rather than leaving an operator to guess.
 * A blank Name on a manifest teammate is the case that made this necessary: the
 * only visible symptom was a disabled button.
 *
 * `editable` decides whether a field counts. A field this host will not accept
 * cannot block a save it is not part of — the pre-#1530 behaviour, when
 * `name` was overlay-only and a manifest teammate's blank one was simply not
 * this form's business.
 */
export function missingRequired(
  draft: AgentDraft,
  editable: (key: AgentFieldKey) => boolean = () => true,
): AgentFieldSpec[] {
  return AGENT_FIELDS.filter(
    (field) => field.required && editable(field.key) && draft[field.key].trim() === "",
  );
}

/**
 * Whether a blank required field should be shown as an ERROR yet, rather than
 * merely marked required.
 *
 * The distinction is about whether the operator has been asked for anything
 * yet. An edit form opens on an existing teammate — Role already filled — so a
 * blank Name is a real gap the moment it appears, and highlighting it is the
 * answer to "why is Save dead?". A fresh Add form is blank everywhere, and
 * painting every box red before a single keystroke is nagging, not help.
 *
 * One rule covers both without either surface tracking "touched": highlight
 * once the form holds *something*. The edit form always does; the Add form
 * starts quiet and lights up the moment the operator types anything.
 */
export function draftHasContent(draft: AgentDraft): boolean {
  return AGENT_FIELDS.some((field) => draft[field.key].trim() !== "");
}

/**
 * The `PATCH` value for a teammate's model-override edit, or `undefined` when
 * the draft did not actually change (issue #1245's per-agent follow-up).
 *
 * Its own function rather than a case inside `agentEdits`: the model lives in
 * its own section of the detail view (admin-gated, separate from the
 * name/role/description group `AGENT_FIELDS` edits together), so it needs its
 * own draft, not a fourth `AgentFieldKey`.
 *
 * Same three-way contract as `description`'s handling inside `agentEdits`,
 * and for the same reason: a blank draft means "clear the override, use the
 * harness's own default" (`null`), not "leave it alone" (`undefined`) — an
 * operator who emptied the field on purpose must see that take effect, not
 * watch the old value silently survive the save.
 */
export function modelEdit(current: string | undefined, draft: string): string | null | undefined {
  const next = draft.trim();
  const before = current ?? "";
  if (next === before) return undefined;
  return next === "" ? null : next;
}

/**
 * The `PATCH` value for a teammate's harness-binding edit, or `undefined`
 * when the draft did not change (issue #1245's harness-picker follow-up).
 *
 * Same three-way contract as [`modelEdit`], and deliberately the sibling
 * function rather than a shared generic: the two fields read alike today, but
 * a harness id is a select's value (never free text an operator can leave
 * whitespace in), so this skips the `.trim()` `modelEdit` needs.
 */
export function harnessEdit(current: string | undefined, draft: string): string | null | undefined {
  const before = current ?? "";
  if (draft === before) return undefined;
  return draft === "" ? null : draft;
}

/**
 * Which declared harness `harnessId` resolves to, and its `kind` — the
 * question the Harness & Model editor needs answered to know whether to show
 * the model field at all (only an `acp` harness has anywhere to forward it).
 *
 * `harnessId` absent or `""` means "the company default", the same contract
 * `AgentDetailDto.harness` and a blank select both carry: resolved against
 * whichever declared harness has `default: true`.
 */
export function resolvedHarnessKind(
  harnesses: HarnessDto[],
  harnessId: string | undefined,
): HarnessDto["kind"] | undefined {
  const id = harnessId || harnesses.find((h) => h.default)?.id;
  return harnesses.find((h) => h.id === id)?.kind;
}

/**
 * A harness's label in the picker's `<select>` — short enough for one line,
 * specific enough that an operator can tell two ACP entries apart by CLI.
 */
export function harnessOptionLabel(harness: HarnessDto): string {
  if (harness.kind === "acp") {
    const cli = harness.agent ?? "external";
    return harness.transport === "runner" ? `${cli} (remote) — ${harness.id}` : `${cli} — ${harness.id}`;
  }
  return `Managed — ${harness.id}`;
}

/** What an agent's tool grants amount to, once the intersection is applied. */
export interface ToolGrantSummary {
  /** What the agent actually holds. */
  effective: string[];
  /**
   * Globs the agent asked for that the company's allow-list does not cover, so
   * they are not grants however plainly the manifest lists them. This is the
   * line an operator checking a tool change is looking for.
   */
  dropped: string[];
  /**
   * Whether the agent lists no tools of its own (`requested === null`) and
   * therefore inherits the company's whole allow-list.
   *
   * Since issue #1804 this is `requested === null`, NOT an empty list: an empty
   * `requested` (`[]`) is now the *opposite* — a deliberate deny-all — so the
   * two states must never be conflated. A screen that read `[]` as the standard
   * grant would tell an operator their locked-down agent holds everything.
   */
  standardGrant: boolean;
  /**
   * Whether the agent was handed an **explicit** empty grant (`requested === []`)
   * — a deliberate deny-all, distinct from `standardGrant`. The teammate holds
   * nothing; the screen must say so rather than fall back to "standard grant".
   */
  deniedAll: boolean;
}

/**
 * Reads an operator-typed tool-grant list into the array the host stores.
 *
 * Split on commas **and** whitespace, because both spellings are what people
 * actually type after reading a `company.toml` — `"docs.*, files.*"` and
 * `"docs.* files.*"` mean the same thing and neither should produce a grant
 * named `"docs.*,"`. Duplicates collapse, order is first-seen, and an entirely
 * blank field is `[]` — which the caller must treat as "the company's standard
 * grant" rather than "no tools".
 */
/** The characters that end a namespace segment in a tool name. */
const TOOL_NAME_SEPARATORS = [".", "_", ":"];

/**
 * The host's `extends_on_boundary`: `name` is `prefix`, or extends it and
 * breaks on a separator.
 *
 * The boundary is the whole point — without it a `docs` grant would cover
 * `documentation.read`, and `file*` would cover `filesystem_wipe`.
 */
function extendsOnBoundary(name: string, prefix: string): boolean {
  if (name === prefix) return true;
  if (!name.startsWith(prefix)) return false;
  if (TOOL_NAME_SEPARATORS.some((sep) => prefix.endsWith(sep))) return true;
  const rest = name.slice(prefix.length);
  return TOOL_NAME_SEPARATORS.some((sep) => rest.startsWith(sep));
}

/**
 * Whether a company's allow-list covers one requested grant glob — the
 * console's mirror of the host's `allow_covers` / `grant_matches` pair, rule
 * for rule.
 *
 * A trailing `*` is stripped from the *request* first (asking for `docs.*` is
 * asking about `docs`), then an allow entry matches if it is the catch-all, an
 * exact hit, or a `*`-suffixed prefix that stops on a namespace boundary. The
 * asymmetries worth knowing, because all of them look like bugs and are not:
 * `workspace.*` does **not** cover a bare `workspace` request (which is why
 * manifests list both); a grant with no trailing `*` matches only itself; and
 * `media`, `composio`, `chargebee`, `hosting`, `paypal`, `search` and the whole
 * `mcp:` namespace are explicit opt-ins that a catch-all `*` never confers —
 * bare namespace or dotted descendant alike — the host's `allow_covers` rejects
 * them under a bare `*`, so this hint must too, or the editor would suppress its
 * "will not apply" warning and save a request the returned detail immediately
 * renders as ineffective.
 *
 * This is a *hint*, shown while an operator types. The host stays the
 * authority: it re-derives `effective` on every read, so a disagreement here
 * shows up as a grant rendered struck through rather than as a permission the
 * console invented.
 */
export function companyCovers(allow: string[], glob: string): boolean {
  const literal = glob.endsWith("*") ? glob.slice(0, -1) : glob;

  // The metered, credentialed, and third-party namespaces are explicit opt-ins
  // on the host: a catch-all `*` never covers them here, even though
  // `grantMatches` treats `*` as a generic match. A belt cannot reintroduce a
  // capability the company intentionally omitted. Workspace writes are also
  // explicit-only because they overwrite operator-owned guidance. A dotted
  // descendant ask (`search.web`, `media.image`, `paypal.wallet`) is as much
  // an opt-in as the bare namespace, so it must not fall through to the generic
  // matcher below either, or a wildcard would look like it covers a grant the
  // host rejects.
  // Likewise a bare `search` grant covers its sub-grant asks, matching
  // `grants_search_explicit`. Workspace writes are explicit-only in *both*
  // spellings the wiring predicate accepts — the bare `workspace` grant as well
  // as `workspace.write` — because a bare `workspace` request under a `["*"]`
  // allow-list would otherwise fall through to the generic matcher below and
  // preview as covered a grant that hands the agent the exact write token.
  //
  // Every opt-in branch also demands the *request* glob be a spelling the
  // wiring predicate accepts when stored verbatim. The write path keeps the
  // request intact in `effective`, so a glued-star ask like `search*` or
  // `workspace.write*` reaches the belt as `search*` / `workspace.write*` —
  // which `grants_search_explicit` and `grants_workspace_write_explicit` both
  // reject, even though their stripped forms would pass. Only the bare
  // namespace, a separator-broken descendant (`search.*`, `search.web`), and
  // the colon forms (`mcp:*`) ever wire; anything else must not preview as
  // covered, or the card would render the saved grant as effective while the
  // tools stay unwired.
  if (literal === "workspace" || literal === "workspace.write") {
    return (
      (glob === "workspace" || glob === "workspace.write") &&
      allow.some((grant) => grant === "workspace" || grant === "workspace.write")
    );
  }
  if (literal === "media" || literal.startsWith("media.")) {
    return grantsExplicit(allow, "media") && grantsExplicit([glob], "media");
  }
  if (literal === "composio" || literal.startsWith("composio.")) {
    return grantsExplicit(allow, "composio") && grantsExplicit([glob], "composio");
  }
  if (literal === "chargebee" || literal.startsWith("chargebee.")) {
    return grantsExplicit(allow, "chargebee") && grantsExplicit([glob], "chargebee");
  }
  if (literal === "hosting" || literal.startsWith("hosting.")) {
    return grantsExplicit(allow, "hosting") && grantsExplicit([glob], "hosting");
  }
  if (literal === "paypal" || literal.startsWith("paypal.")) {
    return grantsExplicit(allow, "paypal") && grantsExplicit([glob], "paypal");
  }
  if (literal === "search" || literal.startsWith("search.")) {
    return grantsExplicit(allow, "search") && grantsExplicit([glob], "search");
  }

  // MCP grants use a colon namespace, so `mcp:*` is the explicit opt-in for an
  // agent asking for all company servers. A bare `*` must not confer it.
  if (literal === "mcp:" || literal.startsWith("mcp:")) {
    return allow.some((grant) => grant !== "*" && grantMatches(grant, literal));
  }
  // A delimiter-free MCP spelling (`mcp`, `mcp*`) is not a form the MCP wiring
  // can honour, so it must not fall through to the generic matcher below: under
  // a wildcard-only allow-list that generic match would accept it, and the
  // saved `mcp*` glob reads on the host as covering every server — the opt-in
  // defeated. Only the colon forms wire; reject the rest of the family here.
  if (literal.startsWith("mcp")) {
    return false;
  }

  return allow.some((grant) => grantMatches(grant, literal));
}

/** The host's `grant_matches`: exact, or a trailing-`*` prefix on a boundary. */
function grantMatches(grant: string, tool: string): boolean {
  if (grant === "*") return true;
  if (grant.endsWith("*")) return extendsOnBoundary(tool, grant.slice(0, -1));
  return grant === tool;
}

/**
 * The host's `grants_<ns>_explicit` family: the bare namespace or any of its
 * sub-grants. A catch-all `*` never confers these.
 */
function grantsExplicit(grants: string[], ns: string): boolean {
  return grants.some((grant) => grant === ns || grant.startsWith(`${ns}.`));
}

export function parseToolGlobs(input: string): string[] {
  const seen = new Set<string>();
  for (const raw of input.split(/[\s,]+/)) {
    const glob = raw.trim();
    if (glob !== "") seen.add(glob);
  }
  return [...seen];
}

/**
 * Whether two grant lists differ as *sets*, so re-ordering or re-spacing a
 * field the operator did not really change does not produce a write.
 */
export function toolGlobsDiffer(before: string[], after: string[]): boolean {
  const beforeSet = new Set(before);
  const afterSet = new Set(after);
  if (beforeSet.size !== afterSet.size) return true;
  return [...afterSet].some((glob) => !beforeSet.has(glob));
}

export function summarizeGrants(tools: AgentToolsDto): ToolGrantSummary {
  // Three-state `requested` (issue #1804): `null` inherits the standard grant,
  // `[]` is a deliberate deny-all, a non-empty array narrows. Only a narrowing
  // list has globs that can be dropped by the intersection.
  const requested = tools.requested ?? [];
  return {
    effective: tools.effective,
    dropped: requested.filter((glob) => !tools.effective.includes(glob)),
    standardGrant: tools.requested === null,
    deniedAll: tools.requested !== null && tools.requested.length === 0,
  };
}

/**
 * The ceiling an editor draft is actually narrowed by — the host's desk level
 * in [`agent_scoped_grants`], mirrored so the preview cannot promise a grant
 * the host drops.
 *
 * `deskAllow` is the host's desk ceiling already intersected with the company
 * grant. **Empty means the narrowed ceiling grants nothing**, which is *not*
 * the same as "no desk narrows anything" — a desk ceiling can resolve to an
 * empty list while still being active (its only grant is an explicit opt-in
 * the company's `*` does not confer). `deskCeilingActive` is the sentinel that
 * tells those apart: when it is true the desk level is the gate, however
 * narrow — even if that means nothing the operator types will apply — and when
 * it is false the company allow-list is. Checking a draft against this list is
 * the exact predicate the host applies when it re-derives `effective`, so a
 * warning drawn from it cannot disagree with the saved result the way one
 * drawn from `companyAllow` alone can — the marketing agency's creative desk
 * omits `media`, while the company allows it, so a draft adding `media` would
 * warn here and then land struck through.
 */
export function grantCeiling(tools: AgentToolsDto): string[] {
  return tools.deskCeilingActive ? tools.deskAllow : tools.companyAllow;
}

/**
 * How an agent's tier reads on screen.
 *
 * `isOrchestrator` rather than `tier === "orchestrator"`: a company that tags
 * nobody still has an orchestrator (the first agent declared), and the host has
 * already resolved which one it is.
 */
export function tierLabel(detail: AgentDetailDto): string {
  return detail.isOrchestrator ? "Orchestrator" : "Worker";
}
