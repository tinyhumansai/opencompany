// The live memory API: the console reads and writes the company's real durable
// facts through the host's `…/memory` routes (REST, camelCase over the wire),
// and reads a `…/memory/stats` health snapshot. Replaces the client-side
// `lib/memory` localStorage stub, so a backend failure can never be masked by
// fake seeded data.

import type { OpenCompanyClient } from "./client";

/** The taxonomy of a durable fact — mirrors the host's `FactKind`. */
export type MemoryKind = "fact" | "preference" | "person" | "project" | "reference";

/**
 * Where a memory row came from — the host's `MemoryOrigin` discriminator.
 * `fact` rows are operator-authored and `document` rows are files or links an
 * operator dropped — both deletable; `agent-memory` and `task-outcome` rows
 * are the agents' own runtime memory and are read-only.
 */
export type MemoryOrigin = "fact" | "agent-memory" | "task-outcome" | "document";

/** One memory row as the host returns it (an operator fact OR an agent chunk). */
export interface MemoryEntry {
  id: string;
  /** The fact taxonomy — present only on `fact` rows (omitted for context). */
  kind?: MemoryKind;
  /** Which backend the row came from; drives editable-vs-read-only rendering. */
  origin: MemoryOrigin;
  /**
   * Whether the operator may delete this row: `fact` rows, and `document`
   * rows they dropped themselves. The agents' own memory is read-only.
   */
  editable: boolean;
  title: string;
  body: string;
  /** Which desk/teammate/agent captured it. */
  source: string;
  /**
   * Epoch-millis of the last update. For context rows this is when the chunk
   * was stored; `0` only when the backend has no stamp for it (chunks written
   * before store times were recorded), which still renders as `—`.
   */
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
  /**
   * The newest epoch-millis across *every* memory source — operator facts and
   * the agents' context chunks alike (`0` when nothing is remembered yet).
   *
   * This, not `factsUpdatedAtMillis`, is what the "Last updated" stat renders:
   * agents only ever write context chunks, so the facts-only figure sits at `0`
   * for any company whose operator has not hand-authored a fact.
   */
  lastUpdatedAtMillis: number;
  /** All displayable memory: facts plus non-mirrored context chunks. */
  totalItems: number;
  /** Context written by teammates, excluding outcomes, documents, and mirrors. */
  teammateMemory: number;
  /** Context chunks from operator-dropped documents/links, disjoint from teammate memory. */
  documentMemory: number;
  /** Stored task outcomes, disjoint from teammate memory. */
  taskOutcomes: number;
}

/**
 * `GET /memory` — the rows plus the context-truncation metadata for the SAME
 * read, so the "showing the newest N of M" notice never compares the capped
 * rows against a count taken at a different moment. The metadata describes the
 * unqueried browse list; a `?query=` request reports it as not applicable.
 */
export interface MemoryList {
  /** The rows: operator facts, then the newest non-mirror context chunks. */
  items: MemoryEntry[];
  /**
   * The non-mirror context chunk population before the 500-row display cap —
   * the "M" in the notice. Facts are never capped, so they are not counted.
   * `0` for `?query=` requests, whose rows are search matches the metadata
   * does not describe.
   */
  totalContext: number;
  /**
   * Whether `items` dropped context rows to the cap, from this same read.
   * Always `false` for `?query=` requests.
   */
  contextTruncated: boolean;
}

/** The kinds in display order, for filters and the add form. */
export const MEMORY_KINDS: MemoryKind[] = ["fact", "preference", "person", "project", "reference"];

/**
 * Per-kind badge styling — identity, not state.
 *
 * The identity palette (`--tone-*`): a memory's kind says what sort of thing
 * it is, never how it is doing. `reference` stays neutral on purpose, as the
 * kind with nothing to distinguish.
 */
export const KIND_STYLES: Record<MemoryKind, string> = {
  fact: "border-tone-2/30 bg-tone-2/10 text-tone-2-text",
  preference: "border-tone-1/30 bg-tone-1/10 text-tone-1-text",
  person: "border-tone-4/30 bg-tone-4/10 text-tone-4-text",
  project: "border-tone-3/30 bg-tone-3/10 text-tone-3-text",
  reference: "border-border bg-muted text-muted-foreground",
};

/** The read-only context origins, in display order (facts filter by kind). */
export const CONTEXT_ORIGINS: Exclude<MemoryOrigin, "fact">[] = [
  "agent-memory",
  "task-outcome",
  "document",
];

/** Human labels for each origin, for badges and the type filter. */
export const ORIGIN_LABELS: Record<MemoryOrigin, string> = {
  fact: "Fact",
  "agent-memory": "Agent memory",
  "task-outcome": "Task outcome",
  document: "Document",
};

/** Per-origin badge styling for the read-only context rows. */
export const ORIGIN_STYLES: Record<Exclude<MemoryOrigin, "fact">, string> = {
  "agent-memory": "border-tone-3/30 bg-tone-3/10 text-tone-3-text",
  // Identity, not status. A task-outcome memory records what happened; it is
  // not itself a failure, which is what the rose it used to wear implied of
  // every one of them.
  "task-outcome": "border-tone-5/30 bg-tone-5/10 text-tone-5-text",
  // Identity, like every other row here: a document memory says where the
  // knowledge came from, not how it is doing. It shares `fact`'s blue on
  // purpose — both are knowledge the operator supplied, as against the two
  // origins beside it, which are the agents' own record of their work.
  document: "border-tone-2/30 bg-tone-2/10 text-tone-2-text",
};

/**
 * The company's memory, newest-first, optionally filtered server-side. The
 * rows come back wrapped with the truncation metadata for the same read.
 */
export function listMemory(
  client: OpenCompanyClient,
  company: string | null,
  opts?: { query?: string; kind?: MemoryKind },
): Promise<MemoryList> {
  const params = new URLSearchParams();
  if (opts?.query) params.set("query", opts.query);
  if (opts?.kind) params.set("kind", opts.kind);
  const qs = params.toString();
  return client.get<MemoryList>(`${client.scopeFor(company)}/memory${qs ? `?${qs}` : ""}`);
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


// ─────────────────────────────────────────────────────────────────────────────
// The memory engine
// ─────────────────────────────────────────────────────────────────────────────

/** One engine the host offers, from `GET …/memory/engine`. */
export interface EngineOption {
  /** `store` | `embedded` | `namespace` | `supermemory` | `mem0` | `cognee` | `null`. */
  id: string;
  label: string;
  description: string;
  /** Whether this build can bind it. A `false` tile renders disabled. */
  available: boolean;
  /** Which feature it needs, when it is not available. */
  unavailableReason?: string;
  requiresUrl: boolean;
  requiresKey: boolean;
  /** `false` only for the null engine, which the picker warns on. */
  durable: boolean;
}

/** The engine surface: bound, saved, and selectable. */
export interface MemoryEngineState {
  /** The engine actually in force right now. */
  active: string;
  capabilities: string[];
  healthy?: boolean;
  /**
   * Mandatory families the engine advertised but did not answer at boot.
   * Empty is healthy; absent means it was never probed.
   *
   * `capabilities` is what the driver claims. This is what the engine
   * answered — the two can disagree, and the bind-time audit cannot catch it
   * for the mandatory families.
   */
  unreachableFamilies?: string[];
  /**
   * Optional families the engine advertised and refused when probed.
   *
   * Reported, never blocking: an engine that cannot serve `people` still
   * serves every cycle. What it does cost is real — the agent tools those
   * families back are offered and fail on their first call — so it belongs on
   * the page rather than only in a log.
   */
  degradedFamilies?: string[];
  /**
   * Families that did not answer inside the probe budget. Not the same verdict
   * as refused — the engine may simply be loaded.
   */
  slowFamilies?: string[];
  /** The engine the saved selection names. */
  selected: string;
  url?: string;
  /** Whether a credential is stored. The bytes never come back. */
  apiKeySet: boolean;
  /** `env` | `config.toml` | `default` — which layer owns the choice. */
  layer: string;
  /** Whether this console may change it (false when the deployment owns it). */
  editable: boolean;
  configPath: string;
  options: EngineOption[];
}

/** What an engine apply did. */
export interface EngineApplied {
  engine: string;
  healthy?: boolean;
  /** Companies still holding the previous engine until a restart. */
  restartRequiredFor: string[];
  configPath: string;
  engineState: MemoryEngineState;
}

/** A probe of a candidate engine, saving nothing. */
export interface EngineProbe {
  /**
   * Whether the candidate can actually be bound. False when it did not answer
   * a health check *or* refused a mandatory family — apply rejects both, so
   * this has to agree with apply rather than only reporting reachability.
   */
  healthy: boolean;
  capabilities: string[];
  /** Mandatory families the candidate refused. Apply will reject these. */
  unreachableFamilies?: string[];
  /** Optional families it refused. Reported, but does not block a bind. */
  degradedFamilies?: string[];
  /** Families that were merely slow. Reported, but does not block a bind. */
  slowFamilies?: string[];
  detail?: string;
}

/** A submitted engine choice. Omit `apiKey` to keep the stored one. */
export interface EngineChoice {
  engine: string;
  url?: string;
  apiKey?: string;
}

/** The engine surface. */
export function memoryEngine(
  client: OpenCompanyClient,
  company: string | null,
): Promise<MemoryEngineState> {
  return client.get<MemoryEngineState>(`${client.scopeFor(company)}/memory/engine`);
}

/** Probes a candidate engine without saving it. */
export function testMemoryEngine(
  client: OpenCompanyClient,
  company: string | null,
  choice: EngineChoice,
): Promise<EngineProbe> {
  return client.post<EngineProbe>(`${client.scopeFor(company)}/memory/engine/test`, choice);
}

/** Saves an engine, binds it, and puts it in force. */
export function applyMemoryEngine(
  client: OpenCompanyClient,
  company: string | null,
  choice: EngineChoice,
): Promise<EngineApplied> {
  return client.put<EngineApplied>(`${client.scopeFor(company)}/memory/engine`, choice);
}

// ─────────────────────────────────────────────────────────────────────────────
// Dropping documents and links into memory
// ─────────────────────────────────────────────────────────────────────────────

/** What happened to one dropped file or link. */
export interface IngestedItem {
  /** The file name, the relative path inside a dropped folder, or the URL. */
  source: string;
  status: "stored" | "empty" | "unsupported" | "failed";
  chunks: number;
  detail?: string;
}

/** A whole drop's outcome — one row per source, never one for the batch. */
export interface Ingested {
  items: IngestedItem[];
  chunks: number;
  stored: number;
}

/**
 * One file on its way to memory: the browser's `File` plus the path it had
 * inside a dropped folder.
 *
 * The path is sent as the part's filename, because it is what the operator
 * sees in their own file manager and the only thing telling four `README.md`s
 * apart.
 */
export interface DroppedFile {
  path: string;
  file: File;
}

/** Uploads one batch of files for extraction into memory. */
export function ingestDocuments(
  client: OpenCompanyClient,
  company: string | null,
  files: DroppedFile[],
): Promise<Ingested> {
  const form = new FormData();
  for (const { path, file } of files) {
    form.append("file", file, path);
  }
  return client.postForm<Ingested>(`${client.scopeFor(company)}/memory/ingest`, form);
}

/** Fetches links host-side and remembers what they said. */
export function ingestLinks(
  client: OpenCompanyClient,
  company: string | null,
  urls: string[],
): Promise<Ingested> {
  return client.post<Ingested>(`${client.scopeFor(company)}/memory/ingest/links`, { urls });
}

/**
 * Forgets every chunk of one dropped document.
 *
 * Keyed by the label slug the host derived, which is what a document row's
 * `id` carries — see `documentSlug`.
 */
export function forgetDocument(
  client: OpenCompanyClient,
  company: string | null,
  slug: string,
): Promise<{ forgotten: number }> {
  return client.del<{ forgotten: number }>(
    `${client.scopeFor(company)}/memory/document/${encodeURIComponent(slug)}`,
  );
}

/**
 * The label slug the host derived for a source name.
 *
 * A copy of the backend's `ingest::label_for`, because a document row carries
 * its chunk address rather than its label and the forget route addresses
 * documents by slug. Kept deliberately narrow — same character class, same
 * 96-character tail — and covered by `memory-slug.test.ts` against the cases
 * the Rust test pins.
 */
export function documentSlug(source: string): string {
  const slug = Array.from(source)
    .map((c) => (/[A-Za-z0-9._-]/.test(c) ? c.toLowerCase() : "-"))
    .join("")
    .replace(/^-+|-+$/g, "");
  const safe = slug || "document";
  return Array.from(safe).length > 96 ? Array.from(safe).slice(-96).join("") : safe;
}
