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

/** Install a skill from the shared registry by slug. */
export function installSkill(
  client: OpenCompanyClient,
  company: string | null,
  slug: string,
): Promise<Skill> {
  return client.post<Skill>(
    `${client.scopeFor(company)}/skills/${encodeURIComponent(slug)}/install`,
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
