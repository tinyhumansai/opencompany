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

function seedMemory(): MemoryEntry[] {
  return [
    entry("preference", "Brand voice is warm and concise", "Lead with value, avoid jargon, keep sentences short.", "Strategy desk"),
    entry("person", "Priya — main client contact", "Approves campaigns on Fridays; prefers a short Loom over long docs.", "Front desk"),
    entry("project", "Spring launch", "Goal: drive signups. Three hero taglines in review; hero image pending.", "Creative studio"),
    entry("fact", "Best-performing channel is email", "Lifecycle email drives ~40% of qualified signups this quarter.", "Analyst"),
    entry("reference", "Positioning one-pager", "Canonical brand positioning lives in Workspace → Brand → Brand voice.md.", "Strategy desk"),
    entry("preference", "No posting on weekends", "Hold social posts for weekday mornings unless flagged urgent.", "Growth desk"),
  ];
}
