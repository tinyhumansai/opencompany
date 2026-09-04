import { ApiError } from "@/api/types";

/**
 * Which of the two New-workflow dialogs is on screen.
 *
 * - `describe` — **one box**: a sentence and Create. Name, Workflow ID,
 *   Description, Nodes and Connections are not rendered at all, and neither is
 *   the validation that serves them. This is what *creating* a workflow is now,
 *   on every company and every build. Create drafts if the copilot can, falls
 *   back to the operator's own sentence if it cannot, and either way lands them
 *   on the canvas.
 * - `form` — the manual graph form, byte-for-byte what the dialog has always
 *   been. Two things reach it: an **edit**, which already has a graph, and a
 *   create the host **refused**, which needs controls for the thing it refused.
 */
export type CreateSurface = "describe" | "form";

/**
 * The **one** place the two dialogs are told apart.
 *
 * A pure function, exported and exhaustively tested, because the failure here is
 * silent in one direction: if this answers `form` on a create, nothing breaks —
 * the dialog just looks like it always did, and nobody notices the redesign
 * never shipped. A predicate spelled inline in the component would be provable
 * only by rendering it, and only for the cases somebody thought to render.
 *
 * ## Why the copilot's availability is not an input
 *
 * It used to be: an `echo` company (no model configured) and a build that
 * answered a capability gap both got the manual form, on the reasoning that a
 * description box is useless with nothing to draft with. Running it proved the
 * opposite — the operator on a host with no model got the full graph form, which
 * is the dialog this redesign exists to retire, and got it in exactly the case
 * where they are least likely to want to hand-author a graph.
 *
 * So the box is unconditional. What changes when the copilot cannot draft is
 * what Create *does*, not what the dialog *is*: the sentence becomes the name
 * and the description, the graph is the blank starter, and the canvas is where
 * it gets built. {@link draftCapabilityGap} still classifies the refusal, but it
 * now feeds the dialog's own copy — so the operator is told the copilot could
 * not draft this — rather than swapping the dialog out from under them.
 *
 * A create the host **refuses** is the one thing that still brings the fields
 * back, and it has to: the refusal that actually happens names an id, and there
 * is no id field on the one-box dialog to obey it with.
 */
export function createSurface(args: {
  /** Edit mode. An edit already has a graph, so there is nothing to draft. */
  editing: boolean;
  /**
   * Whether a one-box create was **refused** by the host.
   *
   * The one that actually happens: the host mints a draft's id by slugging the
   * name and deduping against the workflows it has *saved*
   * (`safe_workflow_id`, `src/harness/built_in/workflow_build.rs`), but nothing
   * reserves it — so two similar descriptions drafted before either is created
   * mint the same id, and the second Create answers
   * `409 A workflow with id ... already exists. Pick a different id.` A dialog
   * with no id field has no way to obey that, so the refusal hands the operator
   * the full form loaded with the graph that was refused. The one-box dialog
   * never dead-ends.
   */
  writeRefused: boolean;
}): CreateSurface {
  if (args.editing) return "form";
  if (args.writeRefused) return "form";
  return "describe";
}

/**
 * The three ways a build can answer "I cannot draft at all", as opposed to
 * "I drafted nothing useful".
 *
 * These are facts about the deployment, not about the description: `not_wired`
 * (404) is a build with no embedded brain, `inference_required` (409) a company
 * with no provider configured, `restart_required` (409) a provider configured
 * since the process booted. None of them is fixed by rewording the sentence, so
 * each one retires *drafting* for this open — the dialog says so in the host's
 * own words, and Create builds the workflow from the operator's sentence
 * instead. The box stays; only the promise above it changes.
 */
const CAPABILITY_CODES = new Set(["not_wired", "inference_required", "restart_required"]);

/**
 * Classifies a failed draft: the host's message when the copilot is
 * **unavailable**, `null` for every other failure.
 *
 * Keyed on the structured `code`, never the prose — the same rule the run
 * refusal banner follows, and for the same reason: a reworded host message must
 * not silently change which dialog an operator sees.
 *
 * A network blip, a 500 or a 400 answers `null` deliberately. Those say nothing
 * about whether this company can draft — retiring the copilot over a dropped
 * connection would leave the operator building by hand on a host that would have
 * drafted it for them a second later.
 */
export function draftCapabilityGap(err: unknown): string | null {
  if (!(err instanceof ApiError)) return null;
  if (!CAPABILITY_CODES.has(err.code)) return null;
  return err.message;
}

/** Cap on a derived name, so a rambling sentence cannot become a 400-character title. */
const NAME_CAP = 60;

/**
 * A workflow name from the sentence the operator typed — the fallback for the
 * one path that has no copilot draft to take a name from.
 *
 * The copilot names its own drafts, so this is used only by "Create it anyway",
 * where the operator has overruled a decline and there is nothing but their
 * sentence to go on. It takes the **first clause** — up to the first sentence or
 * clause break — because that is where an English description says what the
 * thing is, and the rest says how.
 *
 * `"Every Monday, draft the digest and email it."` → `"Every Monday"`. Crude,
 * and deliberately so: the name is renameable on the canvas a second later, and
 * a name that reads like the operator's own words beats one invented for them.
 *
 * Returns `""` when the sentence has nothing usable — the caller must treat that
 * as "no name derived" and ask for one, never write an empty name. An empty name
 * also derives an empty id, and an empty id is the permanent join key nothing
 * can fix afterwards.
 */
export function nameFromDescription(description: string): string {
  const firstClause = description.split(/[.;\n,!?]/, 1)[0] ?? "";
  const collapsed = firstClause.replace(/\s+/g, " ").trim();
  if (!collapsed) return "";
  const capped =
    collapsed.length <= NAME_CAP ? collapsed : `${collapsed.slice(0, NAME_CAP).trimEnd()}…`;
  return capped.charAt(0).toUpperCase() + capped.slice(1);
}
