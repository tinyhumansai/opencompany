// Company-agnostic runtime configuration.
//
// The console works against ANY OpenCompany host and ANY company. Resolution
// order (first match wins), so the same build drops in anywhere:
//   1. URL query params: ?api=<url>&company=<id>&token=<t>
//   2. window.OPENCOMPANY_CONFIG (injected in index.html for static hosting)
//   3. Vite build-time env: VITE_OC_API / VITE_OC_COMPANY / VITE_OC_TOKEN
//   4. Defaults: same-origin API, single-company mode (no id)

export interface ConsoleConfig {
  /** Base URL of the OpenCompany host. Empty string means same-origin. */
  baseUrl: string;
  /**
   * The company id to operate. `null` selects single-company mode, which uses
   * the host's `/api/v1/company/*` aliases for the sole registered company.
   */
  company: string | null;
  /**
   * A **platform** bearer token, for the hosting layer.
   *
   * `null` for humans, which is the normal case: people sign in and the
   * session rides in an HttpOnly cookie. The operator token this once carried
   * no longer exists — there is no shared-secret path into a company.
   */
  operatorToken: string | null;
}

declare global {
  interface Window {
    OPENCOMPANY_CONFIG?: Partial<ConsoleConfig>;
  }
}

function fromQuery(): Partial<ConsoleConfig> {
  const q = new URLSearchParams(window.location.search);
  const out: Partial<ConsoleConfig> = {};
  const api = q.get("api");
  const company = q.get("company");
  const token = q.get("token");
  if (api !== null) out.baseUrl = api;
  if (company !== null) out.company = company;
  if (token !== null) out.operatorToken = token;
  return out;
}

function fromEnv(): Partial<ConsoleConfig> {
  const env = import.meta.env;
  const out: Partial<ConsoleConfig> = {};
  if (env.VITE_OC_API) out.baseUrl = env.VITE_OC_API;
  if (env.VITE_OC_COMPANY) out.company = env.VITE_OC_COMPANY;
  if (env.VITE_OC_TOKEN) out.operatorToken = env.VITE_OC_TOKEN;
  return out;
}

/** Resolves the effective console configuration once, at startup. */
export function resolveConfig(): ConsoleConfig {
  const merged: Partial<ConsoleConfig> = {
    ...fromEnv(),
    ...(window.OPENCOMPANY_CONFIG ?? {}),
    ...fromQuery(),
  };
  // Normalize a trailing slash off the base URL.
  const baseUrl = (merged.baseUrl ?? "").replace(/\/$/, "");
  return {
    baseUrl,
    company: merged.company ?? null,
    operatorToken: merged.operatorToken ?? null,
  };
}
