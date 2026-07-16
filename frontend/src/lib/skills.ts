// The company's skills: capability write-ups (SKILL.md files) the operator can
// view, enable/disable, install from a registry, or add. Persisted per company
// in localStorage; the console has no skills API yet, so this is a local store
// seeded from the company's own skills plus a shared registry.

export type SkillCategory = "Marketing" | "Research" | "Ops" | "Content" | "Finance";

export type SkillSource = "company" | "registry" | "custom";

export interface InstalledSkill {
  id: string;
  name: string;
  description: string;
  category: SkillCategory;
  source: SkillSource;
  enabled: boolean;
}

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

let n = 0;
const genId = () => `skill-${Date.now().toString(36)}-${n++}`;

export function fromRegistry(skill: RegistrySkill): InstalledSkill {
  return { ...skill, source: "registry", enabled: true };
}

export function newSkill(fields: {
  name: string;
  description: string;
  category: SkillCategory;
}): InstalledSkill {
  return {
    id: genId(),
    name: fields.name.trim(),
    description: fields.description.trim(),
    category: fields.category,
    source: "custom",
    enabled: true,
  };
}

const KEY = (company: string | null) => `oc-skills:${company ?? "single"}`;

export function loadSkills(company: string | null): InstalledSkill[] {
  try {
    const raw = localStorage.getItem(KEY(company));
    if (raw) return JSON.parse(raw) as InstalledSkill[];
  } catch {
    /* fall through to seed */
  }
  return seedSkills();
}

export function saveSkills(company: string | null, skills: InstalledSkill[]): void {
  try {
    localStorage.setItem(KEY(company), JSON.stringify(skills));
  } catch {
    /* storage unavailable */
  }
}

/** The company's own skills, from its `skills/` directory. */
function seedSkills(): InstalledSkill[] {
  return [
    { id: "seo-audit", name: "SEO Audit", description: "Audit a site's organic-search health and produce prioritized fixes.", category: "Marketing", source: "company", enabled: true },
    { id: "landing-page", name: "Landing Page", description: "Build and A/B test a conversion-focused landing page from a brief.", category: "Marketing", source: "company", enabled: true },
    { id: "email-campaign", name: "Email Campaign", description: "Design and send a lifecycle or broadcast email program.", category: "Marketing", source: "company", enabled: true },
    { id: "brand-positioning", name: "Brand Positioning", description: "Define a company's positioning on one page.", category: "Marketing", source: "company", enabled: false },
  ];
}
