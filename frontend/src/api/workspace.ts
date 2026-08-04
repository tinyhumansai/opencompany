// The live workspace API: the console reads and writes the company's real
// durable note tree through the host's `…/workspace` routes (REST, camelCase
// over the wire). Replaces the client-side `lib/workspace` localStorage stub —
// the same move `api/memory.ts` made for the Brain — so the operator and the
// agents finally look at one tree instead of two.
//
// Agents reach the same store through the `workspace_list` / `workspace_read` /
// `workspace_write` tools (issue #237), which means a note written by an agent
// shows up here and a note the operator writes here is readable by an agent on
// its next turn.

import type { OpenCompanyClient } from "./client";

/** Whether a node is a folder or a file. */
export type NodeKind = "folder" | "file";

/**
 * One node in the workspace tree, as the host returns it.
 *
 * The tree read carries metadata only — `content` is always absent there, and a
 * body is fetched per file by {@link fetchFile}. `parentId` is normalized to
 * `null` at the workspace root (the host omits the field entirely).
 */
export interface FsNode {
  id: string;
  name: string;
  kind: NodeKind;
  /** null = workspace root. */
  parentId: string | null;
  /** Markdown body. Only ever set on a node built from a file read. */
  content?: string;
  /** Epoch-millis of the last update. */
  updatedAt: number;
}

/** One file's body plus the notes that link to it, from `GET …/workspace/file/{id}`. */
export interface WorkspaceFile {
  id: string;
  name: string;
  content: string;
  updatedAt: number;
  /** Other files whose content links to this one via `[[name]]` — computed by the host. */
  backlinks: FsNode[];
}

/** The wire shape: `parentId` is omitted at the root rather than sent as null. */
interface FsNodeWire extends Omit<FsNode, "parentId"> {
  parentId?: string | null;
}

/**
 * Normalizes a node off the wire. The host omits `parentId` at the workspace
 * root (`skip_serializing_if = "Option::is_none"`), and every tree query in the
 * view keys off `parentId === null`, so an absent field becomes an explicit
 * null exactly once — here — rather than at each call site.
 */
function normalize(node: FsNodeWire): FsNode {
  return { ...node, parentId: node.parentId ?? null };
}

/** Every node in the company's workspace (metadata only; no bodies). */
export async function fetchTree(
  client: OpenCompanyClient,
  company: string | null,
): Promise<FsNode[]> {
  const nodes = await client.get<FsNodeWire[]>(`${client.scopeFor(company)}/workspace`);
  return nodes.map(normalize);
}

/** One file's content and its server-computed backlinks. 404s on a folder id. */
export async function fetchFile(
  client: OpenCompanyClient,
  company: string | null,
  id: string,
): Promise<WorkspaceFile> {
  const file = await client.get<Omit<WorkspaceFile, "backlinks"> & { backlinks: FsNodeWire[] }>(
    `${client.scopeFor(company)}/workspace/file/${encodeURIComponent(id)}`,
  );
  return { ...file, backlinks: file.backlinks.map(normalize) };
}

/** Create a folder or file. The host mints the id and the timestamp. */
export async function createNode(
  client: OpenCompanyClient,
  company: string | null,
  input: { name: string; kind: NodeKind; parentId?: string | null; content?: string },
): Promise<FsNode> {
  const node = await client.post<FsNodeWire>(`${client.scopeFor(company)}/workspace`, input);
  return normalize(node);
}

/** Overwrite a file's content; returns the new last-updated stamp. */
export function writeFile(
  client: OpenCompanyClient,
  company: string | null,
  id: string,
  content: string,
): Promise<{ updatedAt: number }> {
  return client.put<{ updatedAt: number }>(
    `${client.scopeFor(company)}/workspace/file/${encodeURIComponent(id)}`,
    { content },
  );
}

/**
 * Rename and/or move a node, returning the authoritative node.
 *
 * The `parentId` contract is a double option and the distinction is load-bearing:
 * **omit** the key to leave the parent alone (a pure rename), and pass an
 * explicit `null` to move the node back to the workspace root. `JSON.stringify`
 * drops `undefined` properties but keeps `null`, so building the body
 * conditionally — as below — maps straight onto the server's
 * `Option<Option<String>>`. Spreading `{ parentId }` unconditionally would turn
 * every rename into a move-to-root.
 *
 * The move-cycle guard (a folder into its own descendant) is the store's, not
 * the console's: it answers 400 `invalid_request`.
 */
export async function renameMoveNode(
  client: OpenCompanyClient,
  company: string | null,
  id: string,
  changes: { name?: string; parentId?: string | null },
): Promise<FsNode> {
  const body: { name?: string; parentId?: string | null } = {};
  if (changes.name !== undefined) body.name = changes.name;
  if ("parentId" in changes) body.parentId = changes.parentId ?? null;
  const node = await client.patch<FsNodeWire>(
    `${client.scopeFor(company)}/workspace/${encodeURIComponent(id)}`,
    body,
  );
  return normalize(node);
}

/** Delete a node; folders go recursively. */
export function deleteNode(
  client: OpenCompanyClient,
  company: string | null,
  id: string,
): Promise<void> {
  return client.del<void>(`${client.scopeFor(company)}/workspace/${encodeURIComponent(id)}`);
}
