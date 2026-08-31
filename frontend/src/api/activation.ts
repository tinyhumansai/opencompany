// The account-activation funnel API (issue #1843/#1844): the console reads
// the three-step funnel through `GET {scope}/activation` and confirms the
// company's display name through `PATCH {scope}` — the shared surface the
// onboarding gate polls and writes to.
//
// Standalone functions over the shared client (mirrors `api/policy.ts`), so no
// change to `OpenCompanyClient` itself is needed.

import type { OpenCompanyClient } from "./client";

/** The funnel, as the host derives it (`src/company/activation.rs`). */
export interface ActivationStatus {
  /** Whether the operator has confirmed the company's display name. */
  nameConfirmed: boolean;
  /**
   * Whether the company both holds a live Composio connection AND has
   * explicitly granted the `composio` tool namespace — a connection alone
   * does not count, because no agent could use it without the grant.
   */
  integrationConnected: boolean;
  /** Whether a real (non-dry) workflow run has reached `succeeded`. */
  workflowRunSucceeded: boolean;
  /**
   * The latch: `true` once every step above has been true simultaneously, at
   * any point in the company's history — see `activationCompletedAtMillis`.
   * Monotonic: a step regressing later (a connection disconnected) does not
   * flip this back to `false`.
   */
  isActivated: boolean;
  /** Epoch-millis the latch was stamped, absent until it has been. */
  activationCompletedAtMillis?: number;
}

/** The account-activation funnel's current read. */
export function getActivation(
  client: OpenCompanyClient,
  company: string | null,
): Promise<ActivationStatus> {
  return client.get<ActivationStatus>(`${client.scopeFor(company)}/activation`);
}

/** What a successful `PATCH {scope}` name-confirm returns. */
export interface ConfirmCompanyNameResult {
  name: string;
  nameConfirmed: boolean;
}

/**
 * Confirms (or renames) the company's display name — the first activation
 * step. `name` is trimmed and rejected empty on the host; admin-only.
 */
export function confirmCompanyName(
  client: OpenCompanyClient,
  company: string | null,
  name: string,
): Promise<ConfirmCompanyNameResult> {
  return client.patch<ConfirmCompanyNameResult>(`${client.scopeFor(company)}`, { name });
}
