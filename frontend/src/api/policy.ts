// The autonomy-tier API (issue #562): the console reads and writes the
// company's effective `[policy]` through the host's `.../policy` routes (REST,
// camelCase over the wire).
//
// The effective policy is the operator's console override where it sets a
// field, and the committed manifest `[policy]` everywhere else. The console
// never writes the manifest — a rebuild re-persists that from the seed, and for
// `[policy]` that is a deliberate security property — so a change here is a
// durable, attributed *override* the host resolves ahead of the manifest.
//
// Standalone functions over the shared client (mirrors `api/inference.ts`), so
// no change to `OpenCompanyClient` or the shared `api/types.ts` is needed.

import type { OpenCompanyClient } from "./client";

/**
 * One selectable tier, with the host's own words for what it means.
 *
 * The prose comes from the host rather than living here on purpose: it
 * describes what this runtime's approval gate actually does, and a copy in
 * TypeScript would drift from the behaviour it claims to describe. The list
 * also only contains tiers the host accepts, so a console built against a newer
 * or older host offers exactly what that host can honour.
 */
export interface PolicyTier {
  /** The `[policy].mode` word. */
  value: string;
  /** The operator-facing label. */
  label: string;
  /** What choosing it means, in consequences rather than tier vocabulary. */
  description: string;
}

/** The company's effective policy, plus what a reset would restore. */
export interface PolicyStatus {
  /** The tier actually in force. */
  mode: string;
  /**
   * The always-ask list actually in force — the operator's real lever. It wins
   * over every tier, `full` included.
   */
  alwaysApprove: string[];
  /** Spend strictly under this amount without an approval; `null` means no cap. */
  autoApproveUnderUsd: number | null;
  /** How long an undecided approval remains actionable. */
  approvalTtlHours: number;
  /** The manifest's tier, so "reset" can name what it would restore. */
  manifestMode: string;
  /** The manifest's always-ask list, for the same reason. */
  manifestAlwaysApprove: string[];
  /** The manifest's spend cap before a console override. */
  manifestAutoApproveUnderUsd: number | null;
  /** The manifest's configured deadline, or `null` when it uses the default. */
  manifestApprovalTtlHours: number | null;
  /**
   * Whether an operator override is in force.
   *
   * Deliberately not derivable by comparing the values: an override that
   * happens to match the manifest is still an override, and is still what a
   * reset would remove.
   */
  overridden: boolean;
  /** Who set the override, if one is set. */
  setBy?: string;
  /** When it was set (epoch millis), if one is set. */
  setAtMillis?: number;
  /** The selectable tiers, in increasing order of autonomy. */
  tiers: PolicyTier[];
  /**
   * When a change bites, in the host's words.
   *
   * Rendered rather than paraphrased: a tier change lands on the company's
   * NEXT turn, so a turn already running finishes under the previous tier.
   * Since "stop the flood now" is what an operator comes here to do, that gap
   * is worth stating rather than leaving them to discover.
   */
  takesEffect: string;
  /**
   * Every tool name this build's approval gate can match — the complete
   * registry, not the workflow-authorable subset served by
   * `/workflows/tool-slugs`. The "is this a real tool?" note under the field
   * compares against this when the host serves it, so a wired agent tool
   * (`hosting_launch_site`, `publish_artifact`) is never called a mistake just
   * because it cannot be a workflow node. Absent on a host predating the field.
   */
  knownTools?: string[];
}

/**
 * The set-policy body. Omit a field to leave it alone; send `null` to stop
 * overriding it.
 *
 * `alwaysApprove: []` is an operator deliberately clearing the always-ask list,
 * which is NOT the same as omitting the field. Sending neither field is a 422
 * rather than a silent no-op.
 */
export interface SetPolicyInput {
  mode?: string | null;
  alwaysApprove?: string[] | null;
  /** `null` means no spend cap; omit to leave the cap alone. */
  autoApproveUnderUsd?: number | null;
  /** `null` stops overriding the deadline; omit to leave it alone. */
  approvalTtlHours?: number | null;
}

/** The company's effective policy. */
export function getPolicy(
  client: OpenCompanyClient,
  company: string | null,
): Promise<PolicyStatus> {
  return client.get<PolicyStatus>(`${client.scopeFor(company)}/policy`);
}

/** Set the tier and/or the always-ask list. Admin-only, attributed. */
export function setPolicy(
  client: OpenCompanyClient,
  company: string | null,
  body: SetPolicyInput,
): Promise<PolicyStatus> {
  return client.put<PolicyStatus>(`${client.scopeFor(company)}/policy`, body);
}

/** Drop the override so the manifest's `[policy]` applies again. */
export function resetPolicy(
  client: OpenCompanyClient,
  company: string | null,
): Promise<PolicyStatus> {
  return client.del<PolicyStatus>(`${client.scopeFor(company)}/policy`);
}
