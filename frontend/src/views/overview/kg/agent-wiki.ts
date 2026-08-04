// SPDX-License-Identifier: GPL-3.0-or-later

/**
 * The detail card shown when you click a tool on the graph.
 *
 * A tool here is one of two things, and its slug says which: a skill this
 * company has defined (`skill-<id>`), or a single tool advertised by a
 * connected MCP server (`mcp-<serverId>-<toolName>`). Nothing else is invented
 * — the card states what the console actually knows, which is where the tool
 * comes from and who reaches for it.
 */

const SKILL_PREFIX = 'skill-';
const MCP_PREFIX = 'mcp-';

export type ToolWiki = {
  slug: string;
  name: string;
  /** True when the tool is served by an MCP server rather than defined here. */
  mcp: boolean;
  /** Human-readable origin: `MCP server`, `skill`, or a plain tool. */
  kind: string;
  /** Where this tool is managed in the console. */
  path: string;
  summary: string;
  usedBy: string[];
};

/** 'design-review' → 'Design Review' — the fallback when nothing named it. */
export function prettifySlug(slug: string): string {
  return slug
    .split(/[-_]/)
    .map((w) => w.charAt(0).toUpperCase() + w.slice(1))
    .join(' ');
}

/** Whether a slug names a tool served by an MCP server. */
export function isMcpSlug(slug: string): boolean {
  return slug.startsWith(MCP_PREFIX);
}

/**
 * Build a tool's card.
 *
 * `labels` is the adapter's slug → display-name map, built from the skill and
 * MCP records themselves; without it the slug is prettified, which reads badly
 * for an MCP tool (`mcp-fs-1-read_file` → `Mcp Fs 1 Read_file`) and is only a
 * last resort.
 */
export function buildToolWiki(
  slug: string,
  usedBy: string[] = [],
  labels: Record<string, string> = {},
): ToolWiki {
  const mcp = isMcpSlug(slug);
  const skill = slug.startsWith(SKILL_PREFIX);
  const name = labels[slug] ?? prettifySlug(slug);

  return {
    slug,
    name,
    mcp,
    kind: mcp ? 'MCP server' : skill ? 'skill' : 'tool',
    path: mcp ? 'Settings → MCP Servers' : skill ? 'Skills' : '—',
    summary: mcp
      ? `${name} — served by one of this company's connected MCP servers.`
      : skill
        ? `${name} — a skill defined for this company.`
        : `${name} — available to this company.`,
    usedBy,
  };
}
