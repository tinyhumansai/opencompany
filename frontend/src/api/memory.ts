// The live memory API: the console reads and writes the company's real durable
// facts through the host's `…/memory` routes (REST, camelCase over the wire),
// and reads a `…/memory/stats` health snapshot. Replaces the client-side
// `lib/memory` localStorage stub, so a backend failure can never be masked by
// fake seeded data.

import type { OpenCompanyClient } from "./client";

/** The taxonomy of a durable fact — mirrors the host's `FactKind`. */
export type MemoryKind = "fact" | "preference" | "person" | "project" | "reference";

/** One durable memory entry as the host returns it. */
export interface MemoryEntry {
  id: string;
  kind: MemoryKind;
  title: string;
  body: string;
  /** Which desk/teammate captured it. */
  source: string;
  /** Epoch-millis of the last update. */
  updatedAt: number;
}

/** The create-a-memory body; the host mints the id and timestamp. */
export interface CreateMemory {
  kind: MemoryKind;
  title: string;
  body: string;
  source?: string;
}

/**
 * The Brain health snapshot: durable facts plus the agents' runtime context
 * chunks. Lets the console prove the store is live at a glance.
 */
export interface MemoryStats {
  /** Number of durable operator facts. */
  facts: number;
  /** The newest fact's last-updated epoch-millis (`0` when there are none). */
  factsUpdatedAtMillis: number;
  /** Total agent-accessible context chunks (learned context + outcomes + mirrors). */
  agentChunks: number;
  /** Of those, how many are stored task outcomes. */
  taskOutcomes: number;
}

/** The kinds in display order, for filters and the add form. */
export const MEMORY_KINDS: MemoryKind[] = ["fact", "preference", "person", "project", "reference"];

/** Per-kind badge styling. */
export const KIND_STYLES: Record<MemoryKind, string> = {
  fact: "border-sky-500/30 bg-sky-500/10 text-sky-600 dark:text-sky-400",
  preference: "border-violet-500/30 bg-violet-500/10 text-violet-600 dark:text-violet-400",
  person: "border-amber-500/30 bg-amber-500/10 text-amber-600 dark:text-amber-400",
  project: "border-emerald-500/30 bg-emerald-500/10 text-emerald-600 dark:text-emerald-400",
  reference: "border-border bg-muted text-muted-foreground",
};

/** The company's durable facts, newest-first, optionally filtered server-side. */
export function listMemory(
  client: OpenCompanyClient,
  company: string | null,
  opts?: { query?: string; kind?: MemoryKind },
): Promise<MemoryEntry[]> {
  const params = new URLSearchParams();
  if (opts?.query) params.set("query", opts.query);
  if (opts?.kind) params.set("kind", opts.kind);
  const qs = params.toString();
  return client.get<MemoryEntry[]>(`${client.scopeFor(company)}/memory${qs ? `?${qs}` : ""}`);
}

/** Add a durable fact (also mirrored into the agents' recallable context). */
export function createMemory(
  client: OpenCompanyClient,
  company: string | null,
  body: CreateMemory,
): Promise<MemoryEntry> {
  return client.post<MemoryEntry>(`${client.scopeFor(company)}/memory`, body);
}

/** Delete a fact by id. */
export function deleteMemory(
  client: OpenCompanyClient,
  company: string | null,
  id: string,
): Promise<void> {
  return client.del<void>(`${client.scopeFor(company)}/memory/${encodeURIComponent(id)}`);
}

/** The Brain health snapshot. */
export function memoryStats(
  client: OpenCompanyClient,
  company: string | null,
): Promise<MemoryStats> {
  return client.get<MemoryStats>(`${client.scopeFor(company)}/memory/stats`);
}
