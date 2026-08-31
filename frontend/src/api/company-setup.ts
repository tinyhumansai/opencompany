// First-run **company** setup: the one host call the flow needs
// (docs/spec/runtime/company-setup.md).
//
// ## Why this is not `api/setup.ts`
//
// Because that name is taken, by a different feature that means something else
// by "setup". `api/setup.ts` is the **instance** wizard (`/api/v1/setup`,
// `src/server/setup.rs`): host bind, auth mode, brain mode, `config.toml`, which
// company template to boot. This file is the **company** flow
// (`POST {scope}/setup/roster`): three questions, then a roster of agents.
//
// The two landed independently and collided on this path. The instance wizard
// keeps the shared name — it shipped first and its route really is `/api/v1/setup`
// — and this one is spelled out, matching `lib/company-setup.ts` and the spec it
// implements. Anyone reaching for "the setup API" should have to say which.
//
import type { OpenCompanyClient } from "./client";
import type { SetupDraft } from "@/lib/company-setup";

/** One agent the host proposes. Shaped to pass straight to `addTeamMember`. */
export interface ProposedAgent {
  name: string;
  role: string;
  description: string;
  /**
   * The job shape that decides this teammate's tool belt on the host
   * (`AgentFocus` in `src/company/setup.rs`).
   *
   * Carried, never shown and never edited. The console has no business choosing
   * a permission boundary, and round-tripping it untouched is what makes the
   * belt an operator approves on the review screen the belt they get.
   */
  focus?: string | null;
}

/**
 * Why the curated team shipped instead of a designed one
 * (`FallbackReason` in `src/company/setup.rs`).
 *
 * The distinction exists because **the action differs**, and a single sentence
 * covering both can only be vague enough to be useless.
 */
export type RosterFallback =
  /** Nothing was reachable, so no design pass ran. The action is to wire a key. */
  | "no_model"
  /**
   * A builder exists and its call was attempted but never landed — a timeout,
   * or a provider that could not be reached. A model is wired, so the action is
   * to retry or check the provider, not to add a key.
   */
  | "model_unreachable"
  /**
   * A model answered and its answer could not be used — unreadable, too thin to
   * be a company, or the reference team handed back unchanged. Almost always
   * means the answers were too sparse to design from, so the action is to say
   * more about the business. Pointing this operator at a credential would send
   * them to fix something that already worked.
   */
  | "not_designable";

export interface RosterProposal {
  agents: ProposedAgent[];
  /** Which reference team framed the proposal, e.g. `ecommerce`. */
  template: string;
  /**
   * Who wrote this team.
   *
   * `"model"` — designed from the operator's own answers.
   * `"fallback"` — the curated team for this kind of business, shipped whole
   * because no model was reachable, its answer could not be read, or what came
   * back was too thin to be a company.
   *
   * **The dialog says which, and an earlier version did not.** Rendering both
   * identically was defended as "to the operator they are the same thing — a
   * starting point they can edit". That is wrong in the direction that costs
   * trust: someone shown a canned team with no indication assumes a model read
   * their answers and wrote it, and judges the product on a roster it never
   * produced.
   */
  source: "model" | "fallback";
  /**
   * The jobs the operator named, as the **host** split them.
   *
   * Echoed on the review screen so the list the roster was judged against is the
   * list they can see — a bad split is then visible to the person who typed it,
   * rather than silently shaping a prompt.
   */
  jobs?: string[];
  /**
   * The jobs no teammate on this roster owns.
   *
   * Non-empty only when `source` is `"model"`: coverage is a claim the design
   * pass makes and the host checks against its own list. A curated team was
   * chosen by keyword and never read the list, so it claims nothing about it.
   */
  uncovered?: string[];
  /**
   * Why the curated team shipped. Present only when `source` is `"fallback"`.
   *
   * Absent on the `"model"` path, and absent from a host too old to send it —
   * see the dialog's `Fallback` type for why that is not read as `"no_model"`.
   */
  reason?: RosterFallback;
}

/**
 * Ask the host for a starting team.
 *
 * Never throws for a *business* reason — the host answers with the reference
 * team rather than an error when it cannot reach a model, because stranding
 * someone on the setup screen is worse than an imperfect roster. A rejection
 * here is a genuine transport or auth failure, which the caller surfaces as
 * "we couldn't reach your company".
 */
export function proposeRoster(
  client: OpenCompanyClient,
  company: string | null,
  draft: SetupDraft,
): Promise<RosterProposal> {
  return client.post<RosterProposal>(`${client.scopeFor(company)}/setup/roster`, {
    industry: draft.industry.trim(),
    teamHint: draft.teamHint.trim(),
    automate: draft.automate.trim(),
  });
}
