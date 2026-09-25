// Skill presentation data for the console: per-category badge styling.
//
// Both the company's effective skills and the installable shared registry come
// from the host over the `…/skills` API (`@/api/skills`). Nothing about *which*
// skills exist lives on the client — a hardcoded registry array used to live
// here, and it had already drifted from what the backend could actually serve.

export type SkillCategory = "Marketing" | "Research" | "Ops" | "Content" | "Finance";

/**
 * One tint per category — identity, not state.
 *
 * The identity palette (`--tone-*`), not the status one: a skill filed under
 * Content is not "done", and one under Finance is not "failed", which is
 * exactly what the emerald and rose these replaced were saying.
 */
export const CATEGORY_STYLES: Record<SkillCategory, string> = {
  Marketing: "border-tone-1/30 bg-tone-1/10 text-tone-1-text",
  Research: "border-tone-2/30 bg-tone-2/10 text-tone-2-text",
  Ops: "border-tone-5/30 bg-tone-5/10 text-tone-5-text",
  Content: "border-tone-3/30 bg-tone-3/10 text-tone-3-text",
  Finance: "border-tone-4/30 bg-tone-4/10 text-tone-4-text",
};

// What a skill actually is to a teammate (issue #569).
//
// A desk agent can list, describe and read an installed skill, and can never run
// one: `dispatched_belt_excludes_every_deferred_family` pins `run_skill`,
// `skill_run`, `run_workflow` and `await_workflow` off every dispatched belt,
// and only the orchestrator is handed `RunWorkflowTool`. That is deliberate —
// the upstream runner reaches for a global config and bypasses the harness's
// metering — but the tab was built from the vocabulary of switching a capability
// on (install / enable / disable), so an operator reasonably read "enabled" as
// "a teammate will now do this", and nothing on the screen disagreed until they
// tried it and watched nothing happen.
//
// The copy lives here rather than inline in the view so the claim is one string
// with one test on it. What must not regress is the *claim*, not its layout.

/**
 * The Skills tab's standing statement of what installing and enabling a skill
 * does. Says the two things the screen otherwise implies the opposite of:
 * teammates **read** skills, and **running** one is the orchestrator's job.
 */
export const SKILLS_READ_ONLY_NOTE =
  "Skills are reference material your agents read — playbooks they follow, not buttons they press. " +
  "Enabling one makes it available to your agents, and each teammate can be scoped to a subset on its own page; " +
  "executing a saved automation stays the orchestrator's job.";

/**
 * What an installed skill's on/off state means for the company's teammates.
 *
 * Phrased as reach ("available to read") rather than capability ("can use it"):
 * the switch decides whether a skill is visible to an agent, never whether one
 * can execute it.
 *
 * It does not say *every* agent, because that stopped being true once a
 * teammate could be scoped to a subset. This page has no roster, so it cannot
 * name which teammates hold a given skill — the teammate's own page answers
 * that. Claiming "every agent" here would be false for exactly the companies
 * that bothered to scope.
 */
export function skillReachLabel(enabled: boolean): string {
  return enabled ? "Available for your agents to read" : "Hidden from agents";
}

/**
 * The empty-state line the registry tab shows when it has no rows to render
 * (issue #1467).
 *
 * A failed registry read leaves the list empty too, so the naive "this host
 * serves no shared skill registry" asserted a fact about the host derived from
 * the very failure the error alert above already reported — two contradicting
 * claims stacked. When the read failed, `hasError` wins and the line says only
 * that. The "serves no registry" claim is reserved for a read that *succeeded*
 * and came back empty; a non-empty registry filtered to nothing by a search is
 * the third case.
 */
export function registryEmptyLabel(hasError: boolean, registryIsEmpty: boolean): string {
  if (hasError) return "Couldn't reach the registry.";
  if (registryIsEmpty) return "This host serves no shared skill registry.";
  return "No skills match that search.";
}
