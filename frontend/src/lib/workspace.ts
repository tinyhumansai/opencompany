// The team's workspace: a client-side file tree (folders + markdown files) the
// operator can organize, edit, upload to, and delete. Persisted to
// localStorage per company — the console has no file API yet, so this is a
// local working surface (a Drive/Notion-style scratch space).

export interface FsNode {
  id: string;
  name: string;
  kind: "folder" | "file";
  /** null = workspace root. */
  parentId: string | null;
  /** Markdown body for files. */
  content?: string;
  updatedAt: number;
}

let n = 0;
const genId = () => `fs-${Date.now().toString(36)}-${n++}`;

const now = () => Date.now();

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

/* ---- mutations (pure; return a new array) ---- */

export function addFolder(nodes: FsNode[], parentId: string | null, name: string): FsNode[] {
  return [...nodes, { id: genId(), name: name.trim() || "New folder", kind: "folder", parentId, updatedAt: now() }];
}

export function addFile(
  nodes: FsNode[],
  parentId: string | null,
  name: string,
  content = "",
): { nodes: FsNode[]; id: string } {
  const id = genId();
  const fileName = ensureMdExt(name.trim() || "Untitled");
  return {
    nodes: [...nodes, { id, name: fileName, kind: "file", parentId, content, updatedAt: now() }],
    id,
  };
}

export function renameNode(nodes: FsNode[], id: string, name: string): FsNode[] {
  const target = nodeById(nodes, id);
  const next = target?.kind === "file" ? ensureMdExt(name.trim()) : name.trim();
  return nodes.map((x) => (x.id === id ? { ...x, name: next || x.name, updatedAt: now() } : x));
}

export function removeNode(nodes: FsNode[], id: string): FsNode[] {
  const ids = subtreeIds(nodes, id);
  return nodes.filter((x) => !ids.has(x.id));
}

export function moveNode(nodes: FsNode[], id: string, newParentId: string | null): FsNode[] {
  // Never move a folder into itself or a descendant.
  const blocked = subtreeIds(nodes, id);
  if (newParentId && blocked.has(newParentId)) return nodes;
  return nodes.map((x) => (x.id === id ? { ...x, parentId: newParentId, updatedAt: now() } : x));
}

export function setContent(nodes: FsNode[], id: string, content: string): FsNode[] {
  return nodes.map((x) => (x.id === id ? { ...x, content, updatedAt: now() } : x));
}

function ensureMdExt(name: string): string {
  return /\.(md|markdown|txt)$/i.test(name) ? name : `${name}.md`;
}

/* ---- persistence ---- */

const KEY = (company: string | null) => `oc-workspace:${company ?? "single"}`;

export function loadWorkspace(company: string | null): FsNode[] {
  try {
    const raw = localStorage.getItem(KEY(company));
    if (raw) return JSON.parse(raw) as FsNode[];
  } catch {
    /* fall through to seed */
  }
  return seedWorkspace();
}

export function saveWorkspace(company: string | null, nodes: FsNode[]): void {
  try {
    localStorage.setItem(KEY(company), JSON.stringify(nodes));
  } catch {
    /* storage unavailable — keep the in-memory tree */
  }
}

/* ---- seed ---- */

function seedWorkspace(): FsNode[] {
  const campaigns: FsNode = { id: "seed-campaigns", name: "Campaigns", kind: "folder", parentId: null, updatedAt: now() };
  const brand: FsNode = { id: "seed-brand", name: "Brand", kind: "folder", parentId: null, updatedAt: now() };
  return [
    campaigns,
    brand,
    {
      id: "seed-readme",
      name: "README.md",
      kind: "file",
      parentId: null,
      updatedAt: now(),
      content:
        "# Workspace\n\nThe team's shared space. Organize work in **folders**, write in " +
        "**Markdown**, upload files, and keep everything in one place.\n\n- Create folders and files with the toolbar\n" +
        "- Click a file to edit it\n- Use the ⋯ menu to rename, move, or delete\n",
    },
    {
      id: "seed-spring",
      name: "Spring launch.md",
      kind: "file",
      parentId: "seed-campaigns",
      updatedAt: now(),
      content:
        "# Spring launch\n\n## Goals\n- Drive signups from the spring push\n- 3 hero taglines in review\n\n## Checklist\n" +
        "- [x] Brief approved\n- [ ] Taglines drafted\n- [ ] Hero image\n- [ ] Landing page\n\n> Owner: Creative studio\n",
    },
    {
      id: "seed-voice",
      name: "Brand voice.md",
      kind: "file",
      parentId: "seed-brand",
      updatedAt: now(),
      content:
        "# Brand voice\n\nWarm, confident, concise.\n\n| Do | Don't |\n| --- | --- |\n| Speak plainly | Use jargon |\n" +
        "| Lead with value | Bury the point |\n",
    },
  ];
}
