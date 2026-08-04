// The company's memory: durable facts the agents remember — preferences,
// people, projects, and references. Persisted per company in localStorage; the
// console has no memory API yet, so this is a local, editable store.

export type MemoryKind = "fact" | "preference" | "person" | "project" | "reference";

export interface MemoryEntry {
  id: string;
  kind: MemoryKind;
  title: string;
  body: string;
  /** Which desk/agent captured it. */
  source: string;
  updatedAt: number;
}

export const MEMORY_KINDS: MemoryKind[] = ["fact", "preference", "person", "project", "reference"];

export const KIND_STYLES: Record<MemoryKind, string> = {
  fact: "border-sky-500/30 bg-sky-500/10 text-sky-600 dark:text-sky-400",
  preference: "border-violet-500/30 bg-violet-500/10 text-violet-600 dark:text-violet-400",
  person: "border-amber-500/30 bg-amber-500/10 text-amber-600 dark:text-amber-400",
  project: "border-emerald-500/30 bg-emerald-500/10 text-emerald-600 dark:text-emerald-400",
  reference: "border-border bg-muted text-muted-foreground",
};

let n = 0;
const genId = () => `mem-${Date.now().toString(36)}-${n++}`;

export function newMemory(fields: {
  kind: MemoryKind;
  title: string;
  body: string;
  source?: string;
}): MemoryEntry {
  return {
    id: genId(),
    kind: fields.kind,
    title: fields.title.trim(),
    body: fields.body.trim(),
    source: fields.source?.trim() || "You",
    updatedAt: Date.now(),
  };
}

const KEY = (company: string | null) => `oc-memory:${company ?? "single"}`;

export function loadMemory(company: string | null): MemoryEntry[] {
  try {
    const raw = localStorage.getItem(KEY(company));
    if (raw) return JSON.parse(raw) as MemoryEntry[];
  } catch {
    /* fall through to seed */
  }
  return seedMemory();
}

export function saveMemory(company: string | null, entries: MemoryEntry[]): void {
  try {
    localStorage.setItem(KEY(company), JSON.stringify(entries));
  } catch {
    /* storage unavailable */
  }
}

function entry(kind: MemoryKind, title: string, body: string, source: string): MemoryEntry {
  return { id: genId(), kind, title, body, source, updatedAt: Date.now() };
}

/**
 * What a company remembers before anyone has told it anything.
 *
 * Deliberately broad rather than minimal: memory is drawn as a constellation on
 * the Overview graph, and a handful of notes reads as an empty company. Every
 * kind is represented several times over so the folder hubs each have something
 * orbiting them.
 */
function seedMemory(): MemoryEntry[] {
  return [
    // preferences — how this company likes to work
    entry("preference", "Brand voice is warm and concise", "Lead with value, avoid jargon, keep sentences short.", "Strategy desk"),
    entry("preference", "No posting on weekends", "Hold social posts for weekday mornings unless flagged urgent.", "Growth desk"),
    entry("preference", "Ship behind a flag", "Anything user-facing goes out dark first, then gets turned on.", "Engineering"),
    entry("preference", "Plain English in the UI", "No product jargon in labels — say what the button does.", "Design"),
    entry("preference", "Decisions get written down", "If it was decided in a call, it lands in Workspace the same day.", "Ops Lead"),
    entry("preference", "Reply within one business day", "Even if the answer is 'still looking' — silence is the worst reply.", "Support"),

    // people — who we deal with, and how
    entry("person", "Priya — main client contact", "Approves campaigns on Fridays; prefers a short Loom over long docs.", "Front desk"),
    entry("person", "Marcus — procurement", "Owns the contract renewal. Wants pricing in writing before any call.", "Ops Lead"),
    entry("person", "Dana — design partner", "Runs the pilot team. Happy to be quoted; check screenshots first.", "Product Lead"),
    entry("person", "Sam — press contact", "Covers the space quarterly. Give at least a week of lead time.", "Marketer"),

    // projects — what is in flight
    entry("project", "Spring launch", "Goal: drive signups. Three hero taglines in review; hero image pending.", "Creative studio"),
    entry("project", "Billing migration", "Moving to metered plans. Blocked on the webhook spec landing.", "Engineering"),
    entry("project", "Onboarding rewrite", "Cutting the first-run flow from nine screens to four.", "Design"),
    entry("project", "Support macros", "Turning the top twenty questions into one-click answers.", "Support"),

    // facts — what we have measured
    entry("fact", "Best-performing channel is email", "Lifecycle email drives ~40% of qualified signups this quarter.", "Analyst"),
    entry("fact", "Median time to first value is 11 minutes", "From signup to the first real action, measured over the last 90 days.", "Analyst"),
    entry("fact", "Churn concentrates in month two", "Most cancellations land between days 30 and 60, not in the first week.", "Analyst"),
    entry("fact", "Mobile is a third of traffic", "But under a tenth of conversions — the gap is the signup form.", "Researcher"),

    // references — where the canonical version lives
    entry("reference", "Positioning one-pager", "Canonical brand positioning lives in Workspace → Brand → Brand voice.md.", "Strategy desk"),
    entry("reference", "Pricing sheet", "Current list pricing and the discount ladder, in Workspace → Sales.", "Ops Lead"),
    entry("reference", "Incident runbook", "Who to page, in what order, and what to say while you do it.", "Ops Engineer"),
    entry("reference", "Component library", "Every shipped UI pattern, with the one-line rule for when to use it.", "Designer"),
  ];
}
