// A dummy workflow graph for the canvas, in the spirit of OpenHuman's tinyflows
// node kinds (trigger / agent / tool_call / http_request / condition / output).
// This is illustrative sample data — the console has no live flow API yet.

import type { Edge, Node } from "@xyflow/react";

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

function node(
  id: string,
  kind: string,
  name: string,
  summary: string,
  position: { x: number; y: number },
): Node<WorkflowNodeData> {
  const meta = NODE_KIND_META[kind] ?? { emoji: "•", color: "neutral" as const };
  return {
    id,
    type: "oc",
    position,
    data: { kind, name, summary, emoji: meta.emoji, color: meta.color },
  };
}

/** The sample "campaign brief → published post" flow shown on the canvas. */
export const SAMPLE_WORKFLOW: { nodes: Node<WorkflowNodeData>[]; edges: Edge[] } = {
  nodes: [
    node("brief", "trigger", "New campaign brief", "Client drops a brief in the inbox", { x: 0, y: 120 }),
    node("strategist", "agent", "Strategist", "Turns the brief into an angle + outline", { x: 280, y: 120 }),
    node("gate", "condition", "Needs research?", "Branch on topic familiarity", { x: 560, y: 120 }),
    node("research", "tool_call", "Web research", "Pulls sources and competitor takes", { x: 840, y: 20 }),
    node("copy", "agent", "Copywriter", "Drafts the post from the outline", { x: 840, y: 220 }),
    node("design", "agent", "Designer", "Generates the hero image", { x: 1120, y: 220 }),
    node("publish", "http_request", "Publish to CMS", "POST the approved draft", { x: 1400, y: 120 }),
    node("done", "output", "Report back", "Summarize what shipped", { x: 1680, y: 120 }),
  ],
  edges: [
    edge("brief", "strategist"),
    edge("strategist", "gate"),
    edge("gate", "research", "yes"),
    edge("gate", "copy", "no"),
    edge("research", "copy"),
    edge("copy", "design"),
    edge("design", "publish"),
    edge("publish", "done"),
  ],
};

function edge(source: string, target: string, label?: string): Edge {
  return {
    id: `${source}-${target}`,
    source,
    target,
    label,
    animated: true,
  };
}
