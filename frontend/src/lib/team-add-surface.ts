// The Add-teammate dialog's two shapes, and the derivations the reduced one
// needs (issue #1989).
//
// ## Why this is not the workflow dialog's module with the nouns changed
//
// `WorkflowCreateDialog`'s one box works because the host has a
// draft-the-whole-thing route: `draftWorkflowFromDescription` turns a sentence
// into a named, id'd, fully-wired graph before anything is created, so the
// dialog can ask for one thing and still write a complete record.
//
// **There is no such route for a teammate.** The two that exist —
// `draftAgentField` (`/team/<id>/draft`) and `draftNewAgentField`
// (`/team/draft`) — draft ONE field, and only `description` or `instructions`:
// `DraftableField` in `api/agent-copilot.ts` names those two and nothing else.
// `name` and `role` are excluded on purpose, and the reason is in
// `AgentFields.tsx`: "drafting a `name` or a `role` is deliberately not on the
// table — a role is what delegation grounds on, so a drafted one would change
// who the company routes work to."
//
// So the copilot structurally cannot fill this dialog in before the write. What
// it CAN do is fill the teammate in afterwards, on the detail page, where
// `AgentDetailView` already wires both fields to it. That is why the reduced
// dialog creates first and lands the operator on `#/team/<id>?edit` — the
// redirect is not a courtesy at the end of the flow, it is the half of the flow
// that does the drafting.
//
// ## What the reduced dialog therefore asks for
//
// A name and a sentence. Not a sentence alone: with no model in the loop, a
// name could only be derived by splitting the sentence, and a teammate's name
// is not a phrase. `nameFromDescription` in the workflow module yields "Every
// Monday" for a workflow, which reads fine on a canvas; the same split yields
// "Runs paid acquisition" for a teammate, which then renders as a person's name
// on every roster card, in every chat member list and beside every message they
// send. The workflow module can afford that because it is the fallback for one
// rare path ("Create it anyway"); here it would be the only path.
//
// A **role** is a phrase, though, so that same split is the right shape for it
// — and role cannot simply be left blank, because both the teammate's prompts
// are written from it and the detail page's copilot refuses to draft without
// one. See {@link roleFromDescription}.

import type { CognitionPath } from "@/api/inference";

/**
 * Which of the two Add-teammate dialogs is on screen.
 *
 * - `describe` — a name and one box, then Create. Role, What they do,
 *   Instructions, Daily budget and the inbox toggle are not rendered at all.
 *   Create writes the teammate and lands the operator on its detail page with
 *   the edit form open, where the copilot drafts the rest.
 * - `form` — today's full form, unchanged. What a company whose copilot cannot
 *   draft still gets.
 */
export type AddTeammateSurface = "describe" | "form";

/**
 * The **one** place the two dialogs are told apart.
 *
 * A pure function, exported and exhaustively tested, because the failure here
 * is silent in one direction: if the copilot is reachable and this answers
 * `form`, nothing breaks — the dialog just looks like it always did, and nobody
 * reports that the redesign never shipped. A predicate spelled inline in a
 * component is provable only by rendering it, and only for the cases somebody
 * thought to render.
 *
 * ## Why an unsettled cognition read means `describe`
 *
 * `cognition` is `null` both while `/inference` is in flight and on a host with
 * no such route — issue #753 leaves the copilot ENABLED in that case rather
 * than refusing to draft because we could not confirm, and both dialogs that
 * already read it (`AddMemberDialog`, `AgentDetailView`) follow that rule.
 * This follows it too, and the two wrong answers are not symmetrical:
 *
 * - Guessing `describe` on a company that turns out not to draft costs the
 *   operator a teammate whose description they wrote themselves and whose
 *   persona the detail page's copilot then declines to draft, saying so in the
 *   host's own words. Everything they typed is kept, and the teammate is real.
 * - Guessing `form` on a company that CAN draft is silent: it looks exactly
 *   like the dialog did before this change, so nothing reports it.
 *
 * The loud wrong answer is the one to risk.
 *
 * ## Why there is no duplicate-id input, unlike the workflow dialog
 *
 * `WorkflowCreateDialog` needs a `writeRefused` input because the host mints a
 * workflow id by slugging the name without reserving it, so a second create can
 * land a `409` that a dialog with no id field cannot obey. The teammate write
 * has no such dead end: `add_member` mints the agent id through
 * `record.mint_agent_id(&body.name)` (`src/ports/types.rs`), which sweeps
 * `<slug>_2`, `<slug>_3` … until it finds a free one, so two teammates named
 * the same thing both create. The only hand-over this dialog needs is
 * `roleUnderivable`.
 */
export function addTeammateSurface(args: {
  /** The company's cognition path; `null` while unread or on a host without the route. */
  cognition: CognitionPath | null;
  /**
   * Whether a Create was already attempted and the sentence yielded no role.
   *
   * The one dead end the reduced dialog can reach. {@link roleFromDescription}
   * answers `""` for a sentence with no letters or digits in it ("🎉🎉"), and a
   * blank role must never be written — see that function for the three things
   * it breaks. So the operator is handed the full form, carrying what they did
   * type, rather than a Create button that cannot work. The reduced dialog
   * never dead-ends, which is the same promise `WorkflowCreateDialog` makes
   * with its `writeRefused` input.
   */
  roleUnderivable: boolean;
}): AddTeammateSurface {
  // Issue #753: `echo` is the offline brain — there is no model to draft with,
  // so the reduced dialog would create a teammate nothing could finish. That
  // company keeps the full form, which is the operator's decision on #1988
  // applied here: the can't-draft path is hidden, never deleted.
  if (args.cognition === "echo") return "form";
  if (args.roleUnderivable) return "form";
  return "describe";
}

/** Cap on a derived role, so a rambling sentence cannot become a 400-character job title. */
const ROLE_CAP = 60;

/**
 * A role from the sentence the operator typed.
 *
 * ## Why the reduced dialog derives a role at all rather than sending none
 *
 * Blank would be the easy answer and it is the wrong one, in four places that
 * all read `role` and none of which the operator would be told about:
 *
 * 1. **The system prompt interpolates it unguarded.** `persona_prompt`
 *    (`src/company/prompt.rs`) formats `"You are {name}, the {role} at
 *    {company}."` — the neighbouring `description` and `instructions` blocks are
 *    blank-guarded and the role is not, so a blank one ships the teammate a
 *    prompt reading "You are Dana, the  at Acme."
 * 2. **Delegation reads it.** The orchestrator's Team block
 *    (`src/harness/built_in/orchestrator.rs`) and the auto-responder's
 *    channel-member block (`src/harness/built_in/selector.rs`) both render
 *    `id — role`. Routing still grounds on the id, so nothing errors; the model
 *    simply has no job description to choose anyone on.
 * 3. **The detail page's copilot disables itself on a blank role**
 *    (`disabled={saving || cognition === "echo" || !draft.role.trim()}` in
 *    `AgentDetailView.tsx`), and its Save is dead while a required field is
 *    empty. That is precisely the page this dialog hands off to: the operator
 *    would land beside the copilot and find it switched off, over a form that
 *    cannot be saved until they type the field this dialog stopped asking for.
 * 4. **The host will not catch it.** `POST /team` is the one write path that
 *    does not validate the field — `PATCH …/team/{id}`, the orchestrator's
 *    `add_agent` tool, `company.toml` and `agents/<id>.toml` all refuse a blank
 *    role, and the setup roster proposal drops the agent. Nothing in the
 *    repository produces a stored empty role today, so one would be a first.
 *
 * ## Why the first clause
 *
 * Because that is where an English description of a job says what the job is,
 * and the rest says how. "Runs paid acquisition and reports on ROAS." → "Runs
 * paid acquisition". Crude, and deliberately so: unlike a name, a role IS a
 * phrase, so the crude answer is the right *shape*, and the operator lands on a
 * page where the field is one click from edited a second later.
 *
 * It is also the operator's own words rather than a model's, which is the line
 * the copilot is deliberately kept on the other side of.
 *
 * Returns `""` when the sentence has nothing usable. The caller must treat that
 * as "no role derived" and ask for one — never write a blank role, which is
 * case 1–3 above.
 */
export function roleFromDescription(description: string): string {
  const firstClause = description.split(/[.;\n,!?]/, 1)[0] ?? "";
  const collapsed = firstClause.replace(/\s+/g, " ").trim();
  if (!collapsed) return "";
  const capped =
    collapsed.length <= ROLE_CAP ? collapsed : `${collapsed.slice(0, ROLE_CAP).trimEnd()}…`;
  return capped.charAt(0).toUpperCase() + capped.slice(1);
}

/** What the reduced dialog collects, before it is turned into a create. */
export interface DescribedTeammate {
  name: string;
  description: string;
}

/**
 * The teammate a described one amounts to, or `null` when the description
 * yields no role.
 *
 * Its own function so the "what actually gets written" question has a single
 * tested answer, rather than being assembled inline in two dialogs that would
 * then drift. `null` is the hand-over signal: show the full form instead of a
 * Create that would write a role-less teammate.
 */
export function describedTeammateFields(
  described: DescribedTeammate,
): { name: string; role: string; description: string } | null {
  const name = described.name.trim();
  const description = described.description.trim();
  const role = roleFromDescription(description);
  if (!name || !description || !role) return null;
  return { name, role, description };
}
