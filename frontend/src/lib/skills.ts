// Static skill presentation data for the console: the shared registry the
// operator can install from, plus per-category badge styling. The company's
// live effective skills (installed/enabled state) come from the host over the
// `…/skills` API (`@/api/skills`), not from here.

export type SkillCategory = "Marketing" | "Research" | "Ops" | "Content" | "Finance";

export interface RegistrySkill {
  id: string;
  name: string;
  description: string;
  category: SkillCategory;
  publisher: string;
}

export const CATEGORY_STYLES: Record<SkillCategory, string> = {
  Marketing: "border-violet-500/30 bg-violet-500/10 text-violet-600 dark:text-violet-400",
  Research: "border-sky-500/30 bg-sky-500/10 text-sky-600 dark:text-sky-400",
  Ops: "border-amber-500/30 bg-amber-500/10 text-amber-600 dark:text-amber-400",
  Content: "border-emerald-500/30 bg-emerald-500/10 text-emerald-600 dark:text-emerald-400",
  Finance: "border-rose-500/30 bg-rose-500/10 text-rose-600 dark:text-rose-400",
};

/** The shared registry — skills installable into any company. */
export const SKILL_REGISTRY: RegistrySkill[] = [
  { id: "web-research", name: "Web Research", description: "Answer a question from multiple sources with citations.", category: "Research", publisher: "OpenCompany" },
  { id: "weekly-report", name: "Weekly Report", description: "Compile the week's activity into a short report.", category: "Ops", publisher: "OpenCompany" },
  { id: "competitor-analysis", name: "Competitor Analysis", description: "Track rivals' launches, pricing, and positioning.", category: "Research", publisher: "OpenCompany" },
  { id: "social-scheduler", name: "Social Scheduler", description: "Plan and queue posts across social channels.", category: "Marketing", publisher: "OpenCompany" },
  { id: "meeting-notes", name: "Meeting Notes", description: "Turn a transcript into decisions and action items.", category: "Content", publisher: "OpenCompany" },
  { id: "invoice-drafting", name: "Invoice Drafting", description: "Draft invoices from a scope and rate card.", category: "Finance", publisher: "OpenCompany" },
];
