// Pure tree helpers for the team's workspace — a real, durable file tree of
// folders and markdown notes that lives on the **host**, in the company's
// `WorkspaceStore`. The console reads and writes it over the `…/workspace`
// routes in `@/api/workspace`; the company's agents read and write the same
// tree through their workspace tools, so operator and agents share one surface.
//
// Nothing here persists anything. These are the derivations the view needs on
// top of a tree it has already fetched (ordering, ancestry, wiki-link title
// resolution). The one localStorage touch left is the *migration* pair at the
// bottom, which exists solely to rescue notes typed into the retired
// client-side scratchpad and is expected to become a no-op once every browser
// has been through it once.

export type { FsNode } from "@/api/workspace";

import type { FsNode } from "@/api/workspace";

/* ---- queries ---- */

export function childrenOf(nodes: FsNode[], parentId: string | null): FsNode[] {
  return nodes
    .filter((x) => x.parentId === parentId)
    .sort((a, b) => {
      if (a.kind !== b.kind) return a.kind === "folder" ? -1 : 1;
      return a.name.localeCompare(b.name);
    });
}

export function nodeById(nodes: FsNode[], id: string | null): FsNode | undefined {
  return id ? nodes.find((x) => x.id === id) : undefined;
}

/**
 * A file's display title — its name without the markdown extension.
 *
 * Known divergence: this strips `.md`, `.markdown` and `.txt`, while the host's
 * `link_target` (which computes backlinks) strips only `.md`. So a `[[link]]`
 * to a `.txt` note styles as resolved here but is not counted as a backlink
 * server-side. Cosmetic, and deliberately left alone — the seeder only ever
 * creates `.md` notes, and narrowing this would silently restyle existing links.
 */
export function titleOf(node: FsNode): string {
  return node.name.replace(/\.(md|markdown|txt)$/i, "");
}

/** Resolve an Obsidian-style `[[wiki link]]` target to a file, by title. */
export function fileByTitle(nodes: FsNode[], target: string): FsNode | undefined {
  const want = target.trim().toLowerCase();
  return nodes.find((x) => x.kind === "file" && titleOf(x).toLowerCase() === want);
}

/** Ancestor folders (root → current), for breadcrumbs. */
export function pathOf(nodes: FsNode[], id: string | null): FsNode[] {
  const path: FsNode[] = [];
  let cur = nodeById(nodes, id);
  while (cur) {
    path.unshift(cur);
    cur = nodeById(nodes, cur.parentId);
  }
  return path;
}

/** Ids of a node and all its descendants (for delete / move guards). */
export function subtreeIds(nodes: FsNode[], id: string): Set<string> {
  const ids = new Set<string>([id]);
  let grew = true;
  while (grew) {
    grew = false;
    for (const node of nodes) {
      if (node.parentId && ids.has(node.parentId) && !ids.has(node.id)) {
        ids.add(node.id);
        grew = true;
      }
    }
  }
  return ids;
}

/** Notes get a markdown extension unless they already carry a known one. */
export function ensureMdExt(name: string): string {
  return /\.(md|markdown|txt)$/i.test(name) ? name : `${name}.md`;
}

/* ---- migration off the retired localStorage scratchpad ---- */

const KEY = (company: string | null) => `oc-workspace:${company ?? "single"}`;

/**
 * Ids the *bundled seed* used. These four nodes were app source shipped to
 * every browser — marketing copy about a "Spring launch" campaign that belonged
 * to no real company. They carry zero user information, so they are dropped
 * rather than imported: pushing them into the host would inject invented
 * content into every company's genuine workspace.
 */
const SEED_ID_PREFIX = "seed-";

/**
 * Notes a user actually typed into the old client-side workspace, if any.
 *
 * Returns only nodes the *user* authored (the retired `addFile`/`addFolder`
 * minted `fs-…` ids); bundled seed nodes are filtered out. An empty array means
 * there is nothing worth rescuing — either the key is absent, unparseable, or
 * holds nothing but the seed.
 */
export function readLegacyLocalNodes(company: string | null): FsNode[] {
  let raw: string | null = null;
  try {
    raw = localStorage.getItem(KEY(company));
  } catch {
    return [];
  }
  if (!raw) return [];
  let parsed: unknown;
  try {
    parsed = JSON.parse(raw);
  } catch {
    return [];
  }
  if (!Array.isArray(parsed)) return [];
  return parsed.filter(
    (node): node is FsNode =>
      Boolean(node) &&
      typeof (node as FsNode).id === "string" &&
      typeof (node as FsNode).name === "string" &&
      ((node as FsNode).kind === "file" || (node as FsNode).kind === "folder") &&
      !(node as FsNode).id.startsWith(SEED_ID_PREFIX),
  );
}

/** Whether the legacy key exists at all (so a seed-only key can be swept). */
export function hasLegacyLocal(company: string | null): boolean {
  try {
    return localStorage.getItem(KEY(company)) !== null;
  } catch {
    return false;
  }
}

/** Drop the retired scratchpad for this company. */
export function clearLegacyLocal(company: string | null): void {
  try {
    localStorage.removeItem(KEY(company));
  } catch {
    /* storage unavailable — nothing to clear */
  }
}
