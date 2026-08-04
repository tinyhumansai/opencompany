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
