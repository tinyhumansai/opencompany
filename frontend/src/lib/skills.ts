// Skill presentation data for the console: per-category badge styling.
//
// Both the company's effective skills and the installable shared registry come
// from the host over the `…/skills` API (`@/api/skills`). Nothing about *which*
// skills exist lives on the client — a hardcoded registry array used to live
// here, and it had already drifted from what the backend could actually serve.

export type SkillCategory = "Marketing" | "Research" | "Ops" | "Content" | "Finance";

export const CATEGORY_STYLES: Record<SkillCategory, string> = {
  Marketing: "border-violet-500/30 bg-violet-500/10 text-violet-600 dark:text-violet-400",
  Research: "border-sky-500/30 bg-sky-500/10 text-sky-600 dark:text-sky-400",
  Ops: "border-amber-500/30 bg-amber-500/10 text-amber-600 dark:text-amber-400",
  Content: "border-emerald-500/30 bg-emerald-500/10 text-emerald-600 dark:text-emerald-400",
  Finance: "border-rose-500/30 bg-rose-500/10 text-rose-600 dark:text-rose-400",
};
