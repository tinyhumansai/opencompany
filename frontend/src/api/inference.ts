// The live inference API (issue #56 — BYOK): the console reads and writes the
// company's effective inference provider through the host's `.../inference`
// routes (REST, camelCase over the wire). The effective config is the
// highest-precedence of a runtime console override, the committed manifest
// `[inference]`, and the platform managed default.
//
// The outbound credential is WRITE-ONLY: a `key` is sent on set and stored in
// the host's secret store; it is never returned. The read shape carries only a
// `keyConfigured` boolean. Standalone functions over the shared client (mirrors
// `api/skills.ts` / `api/mcp.ts`), so no change to `OpenCompanyClient` or the
// shared `api/types.ts` is needed.

import type { OpenCompanyClient } from "./client";

/** Provider kinds the console offers. */
export type InferenceProvider = "managed" | "openrouter" | "openai_compatible" | "ollama";

/** Where the effective config came from — drives the source badge. */
export type InferenceSource = "managed" | "default" | "manifest" | "runtime";

/** The company's effective inference status. Never carries the credential. */
export interface InferenceStatus {
  /** Provider kind. */
  provider: string;
  /** Telemetry slug: `managed` | `openrouter` | `byok` | `ollama`. */
  slug: string;
  /** Resolved OpenAI-compatible base URL. */
  baseUrl: string;
  /** Abstract-tier → concrete model id. */
  models: Record<string, string>;
  /** Provenance badge. */
  source: InferenceSource;
  /** Whether an outbound key is stored — never the key itself. */
  keyConfigured: boolean;
}

/** The set-provider body. `key` is write-only (never returned). */
export interface SetInferenceInput {
  provider: InferenceProvider;
  baseUrl?: string;
  models?: Record<string, string>;
  /** The outbound credential. Omit to leave unchanged; "" to clear. */
  key?: string;
}

/** A mutating response: the resulting status plus a plain-language note. */
export interface InferenceMutation {
  status: InferenceStatus;
  note: string;
}

/** The live-probe result. */
export interface InferenceTestResult {
  ok: boolean;
  provider?: string;
  note?: string;
  error?: string;
  code?: string;
}

/** The company's effective inference status. */
export function getInferenceStatus(
  client: OpenCompanyClient,
  company: string | null,
): Promise<InferenceStatus> {
  return client.get<InferenceStatus>(`${client.scopeFor(company)}/inference`);
}

/** Set (or replace) the runtime provider override, optionally rotating the key. */
export function setInference(
  client: OpenCompanyClient,
  company: string | null,
  body: SetInferenceInput,
): Promise<InferenceMutation> {
  return client.put<InferenceMutation>(`${client.scopeFor(company)}/inference`, body);
}

/** Clear the runtime override, reverting to the manifest (or managed) config. */
export function revertInference(
  client: OpenCompanyClient,
  company: string | null,
): Promise<InferenceMutation> {
  return client.del<InferenceMutation>(`${client.scopeFor(company)}/inference`);
}

/** Live-probe the resolved provider (one `ping` turn). */
export function testInference(
  client: OpenCompanyClient,
  company: string | null,
): Promise<InferenceTestResult> {
  return client.post<InferenceTestResult>(`${client.scopeFor(company)}/inference/test`, {});
}
