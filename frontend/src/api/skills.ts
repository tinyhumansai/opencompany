// The live skills API: the console reads and writes the company's real
// effective skills through the host's `…/skills` routes (REST, camelCase over
// the wire). The effective set is the company's on-disk bundles unioned with
// the operator's deltas. Replaces the client-side `lib/skills` localStorage
// stub.

import type { OpenCompanyClient } from "./client";

/** An installed skill as the host returns it. */
export interface Skill {
  id: string;
  name: string;
  description: string;
  /** Free-form category (e.g. `Marketing`, `Ops`) — from the skill's doc. */
  category: string;
  /** Provenance: `company` | `registry` | `custom`. */
  source: string;
  enabled: boolean;
  /** The library revision this install snapshotted, when its doc carries one. */
  version?: string | null;
}

/** The author-a-custom-skill body; the host slugs the name into the id. */
export interface CreateSkill {
  name: string;
  description: string;
  category?: string;
  body?: string;
}

/** The company's effective skill set, sorted by slug. */
export function listSkills(client: OpenCompanyClient, company: string | null): Promise<Skill[]> {
  return client.get<Skill[]>(`${client.scopeFor(company)}/skills`);
}

/** One skill in the shared registry, installable into any company.
 *
 * Metadata only — the host never ships a body here, because install resolves
 * content server-side from its own library. */
export interface RegistrySkill {
  id: string;
  name: string;
  /** Free-form category (e.g. `Marketing`, `Ops`) — from the skill's doc. */
  category: string;
  description: string;
  publisher: string;
  /** The library revision this entry ships. Absent on an unversioned skill. */
  version?: string | null;
}

/** The shared registry the operator can install from, live from the host.
 *
 * Empty when the host serves no shared library (platform-provisioned mode). */
export function listRegistrySkills(
  client: OpenCompanyClient,
  company: string | null,
): Promise<RegistrySkill[]> {
  return client.get<RegistrySkill[]>(`${client.scopeFor(company)}/skills/registry`);
}

/** The registry entry's metadata, sent on install.
 *
 * A fallback only: the host resolves a registry slug against its own library and
 * ignores this. It is used solely when the host serves no shared library, where
 * there is nothing to resolve against. */
export interface InstallSkillMeta {
  name: string;
  description: string;
  category?: string;
}

/** Install a skill from the shared registry by slug.
 *
 * The host is authoritative for the content: it persists its own `SKILL.md` for
 * the slug — frontmatter and body verbatim — so the agent gets the whole
 * procedure. `404` means the slug is not in the host's registry. */
export function installSkill(
  client: OpenCompanyClient,
  company: string | null,
  slug: string,
  meta: InstallSkillMeta,
): Promise<Skill> {
  return client.post<Skill>(
    `${client.scopeFor(company)}/skills/${encodeURIComponent(slug)}/install`,
    meta,
  );
}

/** Uninstall a registry or custom skill by slug (a built-in cannot be removed). */
export function uninstallSkill(
  client: OpenCompanyClient,
  company: string | null,
  slug: string,
): Promise<void> {
  return client.post<void>(
    `${client.scopeFor(company)}/skills/${encodeURIComponent(slug)}/uninstall`,
  );
}

/** Toggle a skill on or off. */
export function setSkillEnabled(
  client: OpenCompanyClient,
  company: string | null,
  slug: string,
  enabled: boolean,
): Promise<Skill> {
  return client.put<Skill>(`${client.scopeFor(company)}/skills/${encodeURIComponent(slug)}`, {
    enabled,
  });
}

/** Author a custom skill. */
export function createSkill(
  client: OpenCompanyClient,
  company: string | null,
  body: CreateSkill,
): Promise<Skill> {
  return client.post<Skill>(`${client.scopeFor(company)}/skills`, body);
}

/** What the content scan said about a document, as the host reports it. */
export interface SkillScan {
  verdict: "pass" | "warn" | "block";
  /** One line per finding, in operator-facing language. */
  findings: string[];
  /** Spec rules the document diverges from without being refused. */
  specDeltas: string[];
  /** Whether a blocking verdict was overridden for that one write. */
  forced: boolean;
}

/** What happened to one file in an upload. */
export interface SkillUploadRow {
  /** The file name as it was sent, so a row can be matched to what was dropped. */
  file: string;
  ok: boolean;
  /** The stored skill, with the scan report of the write that stored it. */
  skill?: Skill & { scan?: SkillScan };
  /** Why this file was not stored. */
  error?: string;
  /**
   * Whether that refusal was a blocking scan verdict — the one refusal
   * resending with `force` overrides. The host states it so the override is
   * not offered on a match against the wording of `error`.
   */
  scanBlocked?: boolean;
}

/** Upload `.md` / `.zip` / `.skill` files as skills — one row back per file.
 *
 * A refusal is per file: a malformed file costs its own row and nothing else,
 * so a drop of five files where one is wrong still stores the other four. The
 * promise rejects only when the request as a whole failed. */
export function uploadSkills(
  client: OpenCompanyClient,
  company: string | null,
  files: File[],
  force = false,
): Promise<{ results: SkillUploadRow[] }> {
  const form = new FormData();
  for (const file of files) form.append("file", file, file.name);
  if (force) form.append("force", "true");
  return client.postForm<{ results: SkillUploadRow[] }>(
    `${client.scopeFor(company)}/skills/upload`,
    form,
  );
}

/** One turn of a skill-drafting conversation, as the console holds it.
 *
 * The console owns the transcript and sends it back each turn; the host stores
 * none of it. */
export interface SkillDraftTurn {
  role: "operator" | "copilot";
  text: string;
}

/** One drafted skill, for the operator to keep or throw away. */
export interface SkillDraftAnswer {
  /** What the copilot says. Absent when the pass refused. */
  reply?: string;
  /** The whole `SKILL.md`, never a diff. Absent when this turn asked a question
   * instead of drafting, and when the pass refused — `source` tells those
   * apart. */
  text?: string;
  source: "model" | "unavailable";
  /** Why there is no draft: `no_model`, `budget_exhausted`, `model_unreachable`,
   * `unreadable`, or `refused_by_scan` when the scan refused what came back. */
  reason?: string;
  /** What the scan said about the drafted document, when there was one. */
  scan?: SkillScan;
}

/** Draft a skill document with the company's model. Writes nothing.
 *
 * Only offered when `GET …/inference` reports `designsProfiles` — the host has
 * no drafter on a `sidecar` or `custom` cognition path, and this route can only
 * answer `no_model` there. */
export function draftSkill(
  client: OpenCompanyClient,
  company: string | null,
  messages: SkillDraftTurn[],
): Promise<SkillDraftAnswer> {
  return client.post<SkillDraftAnswer>(`${client.scopeFor(company)}/skills/draft`, { messages });
}
