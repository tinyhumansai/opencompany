// The live MCP-servers API (issue #50): the console reads and writes the
// company's effective MCP tool servers through the host's `.../mcp/servers`
// routes (REST, camelCase over the wire). The effective set is the company's
// committed `[[mcp_server]]` manifest entries unioned with the operator's
// runtime additions/overrides.
//
// Credentials are write-only: a token is sent on add/update and stored in the
// host's secret store; it is never returned. The read shape carries only an
// `authConfigured` boolean. Mirrors `api/skills.ts` (standalone functions over
// the shared client), so no change to `OpenCompanyClient` is needed.

import type { OpenCompanyClient } from "./client";
import type { McpHealth, McpMutationResponse, McpServer, McpToolInfo } from "./types";

/** The auth scheme a write-only credential is stored under. */
export type McpAuthKind = "bearer" | "header" | "query_param";

/** The company's effective MCP servers. */
export function listMcpServers(
  client: OpenCompanyClient,
  company: string | null,
): Promise<McpServer[]> {
  return client.get<McpServer[]>(`${client.scopeFor(company)}/mcp/servers`);
}

/**
 * The add-a-runtime-server body. `token` is write-only (never returned).
 * `authKind` selects how it's stored: `bearer` (default), a custom `header`
 * (with `headerName`), or a `query_param` (with `paramName`).
 */
export interface AddMcpServer {
  name: string;
  endpoint: string;
  description?: string;
  allowedTools?: string[];
  disallowedTools?: string[];
  timeoutSecs?: number;
  token?: string;
  authKind?: McpAuthKind;
  headerName?: string;
  paramName?: string;
}

/** Add a runtime MCP server (optionally with an outbound token). */
export function addMcpServer(
  client: OpenCompanyClient,
  company: string | null,
  body: AddMcpServer,
): Promise<McpMutationResponse> {
  return client.post<McpMutationResponse>(`${client.scopeFor(company)}/mcp/servers`, body);
}

/** The update body — every field optional; only present fields are applied. */
export interface UpdateMcpServer {
  enabled?: boolean;
  endpoint?: string;
  description?: string;
  allowedTools?: string[];
  disallowedTools?: string[];
  timeoutSecs?: number;
  /** Rotate the outbound credential (write-only). Omit to leave it unchanged. */
  token?: string;
  authKind?: McpAuthKind;
  headerName?: string;
  paramName?: string;
}

/** Update a server (enable/disable, tool lists, endpoint, or rotate token). */
export function updateMcpServer(
  client: OpenCompanyClient,
  company: string | null,
  name: string,
  body: UpdateMcpServer,
): Promise<McpMutationResponse> {
  return client.put<McpMutationResponse>(
    `${client.scopeFor(company)}/mcp/servers/${encodeURIComponent(name)}`,
    body,
  );
}

/** Remove a runtime MCP server (a manifest server can only be disabled). */
export function removeMcpServer(
  client: OpenCompanyClient,
  company: string | null,
  name: string,
): Promise<void> {
  return client.del<void>(
    `${client.scopeFor(company)}/mcp/servers/${encodeURIComponent(name)}`,
  );
}

/** Live-discover a server's tools (404 `not_wired` when the harness is off). */
export function discoverMcpTools(
  client: OpenCompanyClient,
  company: string | null,
  name: string,
): Promise<McpToolInfo[]> {
  return client.get<McpToolInfo[]>(
    `${client.scopeFor(company)}/mcp/servers/${encodeURIComponent(name)}/tools`,
  );
}

/**
 * Probe a server on demand and return its (scrubbed) health. 404 `not_wired`
 * when the harness is off.
 */
export function testMcpServer(
  client: OpenCompanyClient,
  company: string | null,
  name: string,
): Promise<McpHealth> {
  return client.post<McpHealth>(
    `${client.scopeFor(company)}/mcp/servers/${encodeURIComponent(name)}/test`,
    {},
  );
}
