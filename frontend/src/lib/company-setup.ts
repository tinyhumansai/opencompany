// First-run company setup: the pure half (docs/spec/runtime/company-setup.md).
//
// Three questions asked once, then a team built on the host. This module holds
// everything decidable without a network or a DOM — the question specs, the
// draft, the per-step validation, and the rule that decides whether to offer
// setup at all — so the dialog stays a rendering concern and the decisions are
// unit-tested.
//
// ## Why the offer rule lives here and not in the controller
//
// "Should we offer setup?" is the one question in this feature that is easy to
// get quietly wrong, and wrong in an expensive direction: offering twice would
// build a second team on top of the first. It is a pure function of the roster
// and the skip flag, so it is pinned in `company-setup.test.ts` rather than
// left to a `useEffect` nobody can assert on.

import type { TeamMemberDto } from "@/api/types";

/** Which question a step asks, and how it is worded. */
export interface SetupStepSpec {
  key: SetupFieldKey;
  /** The question itself, as the operator reads it. */
  question: string;
  /** One line under the question, setting expectations for the answer. */
  hint: string;
  placeholder: string;
  /**
   * Whether an answer is required to move on.
   *
   * Only the first question is. The other two are genuinely skippable: someone
   * who tells us they run a homeware shop and nothing else should still get a
   * team, and a required field there would turn a 40-second flow into a wall.
   */
  required: boolean;
}

export type SetupFieldKey = "industry" | "teamHint" | "automate";

/**
 * The three questions, in order.
 *
 * Every one of them changes what gets built — that is the test applied to
 * anything anyone wants to add here. `industry` picks the reference team and
 * frames the whole call; `teamHint` lets someone name a role we would not have
 * thought of; `automate` is what each agent's mandate is written from.
 */
export const SETUP_STEPS: SetupStepSpec[] = [
  {
    key: "industry",
    question: "What kind of company are you setting up?",
    hint: "A sentence is plenty. What you sell, or what you do.",
    placeholder: "e.g. E-commerce — I sell homeware online",
    required: true,
  },
  {
    key: "teamHint",
    question: "Anyone in particular you need on the team?",
    hint: "Optional. We'll suggest a team either way — this just adds to it.",
    placeholder: "e.g. someone on customer support",
    required: false,
  },
  {
    key: "automate",
    question: "What are you trying to automate?",
    hint: "List whatever comes to mind. This is what your team gets built around.",
    placeholder: "e.g. Meta ads, order dispatch, daily sales reports",
    required: false,
  },
];


/**
 * The jobs the operator named, split the way the **host** splits them.
 *
 * ## This replaced a keyword guess, and that is the point
 *
 * What stood here was `inferSignals` — a regex over a hand-copied duplicate of
 * the host's template keywords, rendering chips like "e-commerce · physical
 * products" under the first question. It claimed the product had *understood*
 * them, before anything had read a word: the chips were never sent anywhere,
 * never reached the prompt, and never touched the roster. And the duplicated
 * keyword list drifted from the host's within a week of being written.
 *
 * This does a smaller thing honestly. It is not inference — it is their own
 * words, split on the separators they typed, shown back so they can see the
 * checklist the roster will be judged against. The host runs the identical rule
 * (`job_items` in `src/company/setup.rs`), and
 * `tests/fixtures/setup-jobs.json` is what stops the two drifting: both test
 * suites read that file, so a change to either rule fails the other's tests.
 *
 * `MAX_JOBS` mirrors the host's cap. Past it a checklist is a backlog.
 */
export const MAX_JOBS = 12;

export function jobItems(automate: string): string[] {
  const items: string[] = [];
  for (const raw of automate.split(/[,;\n\r]/)) {
    const item = raw.trim().replace(/\.+$/, "").trim();
    if (!item) continue;
    if (items.some((seen) => seen.toLowerCase() === item.toLowerCase())) continue;
    items.push(item);
    if (items.length >= MAX_JOBS) break;
  }
  return items;
}

/** The three answers, as the form holds them. */
export type SetupDraft = Record<SetupFieldKey, string>;

export function emptySetupDraft(): SetupDraft {
  return { industry: "", teamHint: "", automate: "" };
}

/**
 * Why this step cannot be left yet, or `undefined` when it can.
 *
 * Deliberately permissive: the only thing that blocks progress is a required
 * question with nothing in it. No length minimums, no "tell us more" — a person
 * who writes three words has answered the question, and the model is given the
 * reference team precisely so that a terse answer still lands a real roster.
 */
export function stepProblem(step: SetupStepSpec, draft: SetupDraft): string | undefined {
  if (step.required && !draft[step.key].trim()) {
    return "Tell us a little about the company first.";
  }
  return undefined;
}

/** Whether every step's answer is good enough to submit. */
export function draftIsSubmittable(draft: SetupDraft): boolean {
  return SETUP_STEPS.every((step) => !stepProblem(step, draft));
}

/**
 * The teammates this company was actually staffed with — everyone on the roster
 * bar the global baseline.
 *
 * ## Why the roster is not the answer on its own (issue #1404)
 *
 * The **global baseline** (`docs/spec/runtime/globals.md`) merges a fixed set of
 * teammates into *every* company at boot, whatever its manifest says, and they
 * cannot be deleted — `DELETE …/team/{id}` answers `409` on each. So
 * `roster.length === 0`, which is what this gate used to ask, is false on every
 * company this product can serve. First-run setup could not open anywhere,
 * including on `companies/e2e_setup` — a company whose whole reason to exist is
 * to reach it.
 *
 * The fix is not to subtract the four ids the baseline ships today. That is a
 * copy of a list which is meant to grow, held somewhere the baseline's authors
 * will never look, and the gate would break silently on the next addition. The
 * host marks provenance on each row instead (`TeamMemberDto.global`, from the
 * same `Agent::global` the merge sets), and this reads it.
 *
 * A host predating that field sends nothing, and `undefined` is read as "not
 * baseline". That is the safe direction: it restores the old behaviour — no
 * offer — rather than offering setup to a company that already has a team.
 */
export function staffedTeam(roster: TeamMemberDto[]): TeamMemberDto[] {
  return roster.filter((member) => member.global !== true);
}

/**
 * Whether nobody has staffed this company yet.
 *
 * Kept as its own named function because the decision to define first-run this
 * way is a product choice rather than an implementation detail — see
 * {@link shouldOfferSetup}. It is deliberately *not* "the roster is empty": see
 * {@link staffedTeam} for why that question can no longer be asked.
 */
export function teamIsUnstaffed(roster: TeamMemberDto[]): boolean {
  return staffedTeam(roster).length === 0;
}

/**
 * Whether to open setup unprompted.
 *
 * **The team is the signal**, not a stored "has run" flag. A flag in the browser
 * would re-fire after cleared storage or on a second device and build a
 * duplicate team; asking the host who is on the roster cannot drift, because it
 * is the same thing setup changes.
 *
 * Decision D4 is intact and the question is narrower: not "does this company
 * have any staff" but "does it have any staff *somebody chose for it*". The
 * global baseline is what every company has by definition, so it is evidence of
 * nothing — counting it is what made this gate answer `no` on every company
 * (issue #1404).
 *
 * The cost of this rule, accepted deliberately: a company whose manifest names
 * agents of its own — which is every company under `companies/` except
 * `companies/e2e_setup` — never sees the offer, because it was never unstaffed.
 * Those companies came with a team, so there is nothing for setup to do. The
 * Team page's in-place prompt, Settings, and `#/setup` are ways back after a
 * skip or when an operator wants to run it manually.
 *
 * `skipped` suppresses only the *unprompted* open. It is browser-local and that
 * is safe precisely because it can only ever hide an offer, never cause a
 * second team to be built.
 */
export function shouldOfferSetup({
  roster,
  skipped,
}: {
  roster: TeamMemberDto[];
  skipped: boolean;
}): boolean {
  return teamIsUnstaffed(roster) && !skipped;
}

/**
 * Whether the team page should keep offering setup in place.
 *
 * The other half of "blocking but skippable": skipping must not be a dead end,
 * so an unstaffed company keeps a visible way back in even after the dialog has
 * been dismissed. Same test, minus the skip suppression.
 */
export function shouldPromptSetup(roster: TeamMemberDto[]): boolean {
  return teamIsUnstaffed(roster);
}

/** Progress copy for the build-out screen: `Creating your team… 2 of 5`. */
export function buildOutLabel(created: number, total: number): string {
  return total > 0 ? `${Math.min(created, total)} of ${total}` : "";
}

/**
 * Why this address cannot be a company admin, or `undefined` when it can.
 *
 * ## Caught where it is typed, not where it fails
 *
 * The email step only checked for emptiness, so `as` walked through it. The
 * failure then surfaced from the **manifest validator on the last screen** —
 * after the roster had been designed and the apply attempted — as "that didn't
 * apply: `[users].admins` has an invalid entry". The operator was shown a
 * configuration error about a mistake they had made four steps earlier, in
 * language belonging to a file they have never seen.
 *
 * ## Loose on purpose, and it must stay loose
 *
 * This mirrors `is_usable_admin_email` in `src/ports/users.rs`: trim, lowercase,
 * and demand an `@`. It is not a mail-server-grade check and must not become
 * one. The host's rule exists to stop an entry normalizing into something
 * `LoginIdentity::parse` reads as the local-owner identity rather than an email
 * admin — not to police what a mail server accepts. A console applying a
 * stricter regex would refuse addresses this host takes everywhere else, which
 * is the same bug pointing the other way.
 *
 * `tests/fixtures/setup-admin-email.json` is read by this rule's test and by the
 * host's, which is what keeps the two re-implementations honest.
 *
 * @param required whether an address is needed at all — false on a host with no
 * sign-in, where blank is a fine answer but a typo still is not.
 */
export function adminEmailProblem(value: string, required: boolean): string | undefined {
  const normalized = value.trim().toLowerCase();
  if (!normalized) {
    return required
      ? "We need an address, or nobody will be able to sign in to this company."
      : undefined;
  }
  if (!normalized.includes("@")) {
    return "That doesn't look like an email address — it needs an @.";
  }
  return undefined;
}
