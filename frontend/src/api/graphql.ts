/**
 * The typed unwrap around {@link OpenCompanyClient.graphqlRequest}.
 *
 * No GraphQL client library, deliberately. `graphqlRequest` already POSTs
 * through this client's pluggable `Transport`, so it inherits the auth headers,
 * the carried session and the `ApiError` mapping the rest of the console uses —
 * and the desktop path, which routes through Tauri IPC rather than `fetch`,
 * works unchanged. Apollo or urql would each bring a normalizing cache, a
 * codegen step and a build-time schema dependency to serve one view.
 *
 * What this file adds is the one thing `graphqlRequest` deliberately does not
 * do: **check the `errors` array**. GraphQL answers a failed query with HTTP
 * 200 and a populated `errors`, so the client's own `isOk` check passes and a
 * caller reading `data` silently gets `undefined`. That renders as a blank
 * panel with nothing in the console and no failed request in the network tab —
 * which is a bad hour for whoever debugs it.
 */

import type { OpenCompanyClient } from "@/api/client";
import { ApiError } from "@/api/types";

/** One entry of a GraphQL `errors` array, as much of it as we rely on. */
interface GraphQLErrorEntry {
  message?: string;
  path?: (string | number)[];
}

/**
 * Wire messages the read plane returns as bare codes, and what they mean to
 * whoever is looking at the panel.
 *
 * A GraphQL error message is a protocol token, not prose. Rendering one
 * verbatim puts a word like `unauthorized` on screen next to a retry that
 * repeats the identical request and therefore fails identically — it names
 * neither what went wrong nor what would fix it.
 */
const WIRE_MESSAGES: Record<string, string> = {
  unauthorized: "Your session didn't cover this company. Sign in again to reload it.",
  forbidden: "This company's data isn't visible to your account.",
};

/** Flattens an `errors` array into one operator-facing sentence. */
function describe(errors: GraphQLErrorEntry[]): string {
  const messages = errors
    .map((e) => (typeof e?.message === "string" ? e.message : ""))
    .filter(Boolean)
    .map((m) => WIRE_MESSAGES[m] ?? m);
  if (messages.length === 0) return "the query was refused";
  return messages.join("; ");
}

/** Whether `errors` says the caller was refused rather than the query failed. */
function isRefusal(errors: GraphQLErrorEntry[]): boolean {
  return errors.some((e) => e?.message === "unauthorized" || e?.message === "forbidden");
}

/**
 * Runs `document` and returns its `data`, narrowed to `T`.
 *
 * Throws an {@link ApiError} when the response carries errors — including a
 * *partial* success, where `data` is present alongside them. Partial data from
 * a resolver that failed is the shape most likely to be rendered as though it
 * were whole, so it is refused rather than passed on.
 */
export async function runQuery<T>(
  client: OpenCompanyClient,
  document: string,
  variables?: Record<string, unknown>,
  company?: string | null,
): Promise<T> {
  const response = await client.graphqlRequest(document, variables, company);
  const errors = Array.isArray(response?.errors)
    ? (response.errors as GraphQLErrorEntry[])
    : [];
  // Status 200 on purpose, because that is genuinely what came back: GraphQL
  // reports a refused query in the body, not the status line. `structured` is
  // true for the same reason `ApiError` draws that distinction elsewhere — the
  // host considered this request and refused it, which is not the same as
  // something between the browser and the host giving up.
  if (errors.length > 0) {
    // A distinct code, because a refusal and a failed read need different
    // offers: retrying a refusal repeats it exactly.
    const code = isRefusal(errors) ? "graphql_refused" : "graphql_error";
    throw new ApiError(200, code, describe(errors), true);
  }
  if (response?.data === undefined || response.data === null) {
    throw new ApiError(200, "graphql_error", "the query returned no data", true);
  }
  return response.data as T;
}
