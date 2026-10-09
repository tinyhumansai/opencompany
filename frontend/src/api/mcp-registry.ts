// The MCP **directory** routes (issue #1270): browse the two upstream registries
// the host queries (the open `modelcontextprotocol/registry`), install
// an entry, and drive an install's lifecycle and credentials.
//
// Sibling of `api/mcp.ts`, which serves List A — the company's `[[mcp_server]]`
// manifest entries and the URLs an operator typed in. The two lists come back
// merged from one read (`listMcpServers`); this module is only the write and
// browse half that List A structurally cannot do.
//
// ## Two keys, and they are not interchangeable
//
// List A addresses a server by `name`. A directory install is addressed by its
// `serverId` — a stable id in OpenHuman's own store — and its `name` is a
// display slug the host mints for the merged row. Sending one where the other
// belongs deletes or rotates the wrong server, so every function here takes a
// `serverId` and none takes a name.
//
// ## Credentials are write-only in both directions
//
// An entry declares the env keys it needs; their values go out on install and on
// rotation and never come back. Nothing in this module returns one, and the
// merged row reports only a non-secret `authConfigured` boolean.
//
// ## `not_wired`
//
// Every route here is behind the host's `mcp` Cargo feature. A build without it
// registers the same routes and answers `404 not_wired` — a fact about the
// build, not a failure. Callers classify that with
// [`registryOutage`](@/lib/mcp-registry) rather than showing the raw error.

import type { OpenCompanyClient, RequestOptions } from "./client";
import { ApiError } from "./types";
import type { McpHealth, McpServer } from "./types";
import type { McpAuthKind } from "./mcp";

/** One directory listing, as `GET …/mcp/registry/search` returns it. */
export interface McpCatalogueEntry {
  /** The directory's stable identity (`@org/server`) — what an install is keyed on. */
  qualifiedName: string;
  displayName: string;
  description?: string;
  iconUrl?: string;
  /** Which upstream directory this row came from (`mcp_official`). */
  source: string;
  /** Upstream's canonical-first-party badge. */
  official: boolean;
  useCount: number;
  websiteUrl?: string;
}

/** A page of directory results. */
export interface McpCatalogueSearch {
  servers: McpCatalogueEntry[];
  page: number;
  totalPages: number;
}

/** A mutating response: the resulting status plus a plain-language note. */
/**
 * One directory entry in full, with the install decision already made by the
 * host so the console never offers an install that would be refused.
 */
export interface McpCatalogueDetail {
  qualifiedName: string;
  displayName: string;
  description?: string;
  iconUrl?: string;
  source: string;
  /** The hosted endpoint an install would dial. Absent ⇒ nothing dialable here. */
  endpoint?: string;
  /**
   * The env keys the install form must collect, as the host derived them from
   * the connection the install will actually use. Values are write-only.
   */
  requiredEnvKeys: string[];
  /** Whether `POST …/mcp/registry/install` would accept this entry. */
  installable: boolean;
  /** Why not, when it would not. Present iff `installable` is false. */
  refusal?: string;
}

/**
 * A registry mutation's answer: the resulting **merged** row and the connection
 * state right after the change.
 *
 * `server` is re-read from the merged list, so an install that reconciles onto
 * a manifest or runtime row comes back badged as that row — the same one a
 * following `GET …/mcp/servers` will show.
 */
export interface McpRegistryMutation {
  server: McpServer;
  note: string;
  /**
   * The connection state after the mutation. A refused connection is **not** a
   * rollback: an install that lands "needs a credential" is at a valid resting
   * state, the same way `POST …/mcp/servers` treats a `needs_config` probe.
   */
  test?: McpHealth;
}

/**
 * A search body, or a stated failure.
 *
 * Same guard as [`expectList`](./mcp.ts) and for the same reason: `client.get<T>`
 * casts an unparsed body straight to `T`, so a declared type is a claim about
 * the host and never a check on it. A body whose `servers` is not an array would
 * otherwise reach the render and throw on `.map` several frames from the fetch.
 */
export function expectCatalogue(body: unknown): McpCatalogueSearch {
  const page = body as Partial<McpCatalogueSearch> | null | undefined;
  if (!page || typeof page !== "object" || !Array.isArray(page.servers)) {
    // Status 0: nothing was refused, the answer was unusable.
    throw new ApiError(0, "unexpected_shape", "the host's MCP directory results weren't a list");
  }
  return {
    servers: page.servers,
    page: typeof page.page === "number" ? page.page : 1,
    totalPages: typeof page.totalPages === "number" ? page.totalPages : 0,
    // An unrecognised tier degrades to `none`, which only ever offers the
    // operator MORE explanation than the truth. Guessing `company` from an
    // unknown string would tell them their key is working when it may not be.
  };
}

/** Narrows an unvalidated tier string; anything unknown reads as `none`. */

/** Browse the upstream directories. An empty `q` is the directory's own front page. */
export async function searchMcpRegistry(
  client: OpenCompanyClient,
  company: string | null,
  query: { q?: string; page?: number; pageSize?: number },
  options?: RequestOptions,
): Promise<McpCatalogueSearch> {
  const params = new URLSearchParams();
  if (query.q?.trim()) params.set("q", query.q.trim());
  if (query.page !== undefined) params.set("page", String(query.page));
  if (query.pageSize !== undefined) params.set("pageSize", String(query.pageSize));
  const suffix = params.toString();
  const body = await client.get<unknown>(
    `${client.scopeFor(company)}/mcp/registry/search${suffix ? `?${suffix}` : ""}`,
    options,
  );
  return expectCatalogue(body);
}

/** One directory entry in full — the env keys an install must collect live here. */
export function getMcpRegistryEntry(
  client: OpenCompanyClient,
  company: string | null,
  qualifiedName: string,
  options?: RequestOptions,
): Promise<McpCatalogueDetail> {
  return client.get<McpCatalogueDetail>(
    `${client.scopeFor(company)}/mcp/registry/entry?qualifiedName=${encodeURIComponent(qualifiedName)}`,
    options,
  );
}

/**
 * Declare a directory entry as one of this company's own servers.
 *
 * The host writes the same runtime index `POST …/mcp/servers` writes, so the
 * result is an ordinary `runtime`-sourced row.
 *
 * The credential is supplied the way the add-server form supplies one — a
 * `token` read by `authKind` — and is write-only: no route here reads one
 * back. It is deliberately not the entry's `requiredEnvKeys` map: those name a
 * launcher's environment, and a hosted endpoint has no launcher, so how the
 * secret travels is chosen rather than guessed. Show the keys as guidance.
 */
export function installMcpRegistryEntry(
  client: OpenCompanyClient,
  company: string | null,
  body: {
    qualifiedName: string;
    token?: string;
    authKind?: McpAuthKind;
    headerName?: string;
    paramName?: string;
  },
): Promise<McpRegistryMutation> {
  return client.post<McpRegistryMutation>(
    `${client.scopeFor(company)}/mcp/registry/install`,
    body,
  );
}

/** Dial an installed server. Keyed by `serverId`, never by the row's name. */
export function connectMcpRegistryServer(
  client: OpenCompanyClient,
  company: string | null,
  serverId: string,
): Promise<McpRegistryMutation> {
  return client.post<McpRegistryMutation>(
    `${client.scopeFor(company)}/mcp/registry/${encodeURIComponent(serverId)}/connect`,
    {},
  );
}

/** Drop the live session, keeping the install and its stored credentials. */
export function disconnectMcpRegistryServer(
  client: OpenCompanyClient,
  company: string | null,
  serverId: string,
): Promise<McpRegistryMutation> {
  return client.post<McpRegistryMutation>(
    `${client.scopeFor(company)}/mcp/registry/${encodeURIComponent(serverId)}/disconnect`,
    {},
  );
}

/**
 * Rotate an install's credentials (write-only).
 *
 * The host merges the supplied keys over the stored ones and reconnects, so a
 * form that sends only the field the operator retyped does not erase the rest.
 */
export function updateMcpRegistryEnv(
  client: OpenCompanyClient,
  company: string | null,
  serverId: string,
  env: Record<string, string>,
): Promise<McpRegistryMutation> {
  return client.put<McpRegistryMutation>(
    `${client.scopeFor(company)}/mcp/registry/${encodeURIComponent(serverId)}/env`,
    { env },
  );
}

/**
 * Uninstall a directory install and drop its stored env values.
 *
 * This is the **only** delete a `registry`-badged row may use.
 * `DELETE …/mcp/servers/{name}` is List A's, keyed on a declaration a registry
 * row does not have.
 */
export function uninstallMcpRegistryServer(
  client: OpenCompanyClient,
  company: string | null,
  serverId: string,
): Promise<void> {
  return client.del<void>(
    `${client.scopeFor(company)}/mcp/registry/${encodeURIComponent(serverId)}`,
  );
}
