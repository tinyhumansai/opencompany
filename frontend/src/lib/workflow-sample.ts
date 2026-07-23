// Presentation metadata for the workflow canvas, keyed by the tinyflows node
// kinds (trigger / agent / tool_call / http_request / condition / output). The
// live graph comes from the host (`@/api/workflows`); this module only maps each
// kind to its emoji + accent so `WorkflowsView` and `WorkflowNode` render it.

export type NodeColor = "primary" | "sage" | "amber" | "coral" | "neutral";

export interface WorkflowNodeData extends Record<string, unknown> {
  kind: string;
  name: string;
  summary: string;
  emoji: string;
  color: NodeColor;
}

/** Per-kind emoji + accent, mirroring OpenHuman's node-kind metadata. */
export const NODE_KIND_META: Record<string, { emoji: string; color: NodeColor }> = {
  trigger: { emoji: "⚡", color: "sage" },
  agent: { emoji: "🤖", color: "primary" },
  tool_call: { emoji: "🔧", color: "amber" },
  http_request: { emoji: "🌐", color: "coral" },
  condition: { emoji: "🔀", color: "primary" },
  output: { emoji: "📋", color: "amber" },
};

/** Tailwind classes per accent, so light/dark theming comes from tokens. */
export const COLOR_CLASSES: Record<NodeColor, { border: string; chip: string }> = {
  primary: { border: "border-primary/40", chip: "bg-primary/10" },
  sage: { border: "border-emerald-500/40", chip: "bg-emerald-500/10" },
  amber: { border: "border-amber-500/40", chip: "bg-amber-500/10" },
  coral: { border: "border-rose-500/40", chip: "bg-rose-500/10" },
  neutral: { border: "border-border", chip: "bg-muted" },
};

/** Emoji + accent for a node kind, falling back for an unknown kind. */
export function nodeKindMeta(kind: string): { emoji: string; color: NodeColor } {
  return NODE_KIND_META[kind] ?? { emoji: "•", color: "neutral" };
}
