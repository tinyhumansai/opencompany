// The agent graph: the whole company as one sunburst, filling the page.
//
// The company sits at the centre, its hubs — teammates, skill categories, MCP
// servers — on ring 1, and everything those hold on ring 2. Hovering lights the
// full chain through a node so one glance answers "whose is this"; clicking
// dives, which moves the camera onto that node rather than redrawing the scene,
// so you never lose where it sits in the whole.
//
// Labels are counter-scaled against the zoom, so type stays the same size on
// screen at every depth and only the diagram grows.

import { useEffect, useMemo, useState } from "react";
import {
  Blocks,
  Building2,
  Circle,
  Sparkles,
  SquareKanban,
  UserRound,
  Wrench,
  type LucideIcon,
} from "lucide-react";

import { cn } from "@/lib/utils";
import { BRANCH_MARK } from "./palette";
import {
  BRANCH_OF,
  chainOf,
  COMPANY_ID,
  layoutGraph,
  type Graph,
  type NodeKind,
} from "./graph";

// A square frame: the sunburst is radial, so any extra width is dead margin
// that only shrinks the diagram when the SVG is scaled to fit.
const W = 780;
const H = 780;
const CX = W / 2;
const CY = H / 2;
const HUB_RING = 192;
const LEAF_RING = 302;

/** How far the camera pushes in at each depth. */
const ZOOM = [1, 1.7, 2.6];

const EASE = "cubic-bezier(0.22, 1, 0.36, 1)";
const MOVE = { transition: `transform 560ms ${EASE}` } as const;
const FADE = { transition: `opacity 300ms ${EASE}` } as const;

/** The icon each kind wears. Shape carries identity where colour cannot. */
export const KIND_ICON: Record<NodeKind, LucideIcon> = {
  company: Building2,
  desk: UserRound,
  card: SquareKanban,
  capability: Sparkles,
  skill: Circle,
  server: Blocks,
  tool: Wrench,
};

interface Props {
  graph: Graph;
  /** The node the camera is on, or the company when nothing is dived into. */
  focusId: string;
  onFocus: (id: string) => void;
}

export function AgentGraph({ graph, focusId, onFocus }: Props) {
  const [hoverId, setHoverId] = useState<string | null>(null);

  const placed = useMemo(
    () => layoutGraph(graph, { cx: CX, cy: CY, hubRing: HUB_RING, leafRing: LEAF_RING }),
    [graph],
  );

  // A focus can outlive its node — across a poll, or because the lens hid its
  // kind. Fall back to the whole company rather than pointing at nothing; the
  // caller keeps the id, so re-showing the kind restores the focus.
  const focus = placed.has(focusId) ? focusId : COMPANY_ID;

  const rings = useMemo(() => {
    const occupied = new Set([...placed.values()].map((p) => p.ring));
    return [occupied.has(1) && HUB_RING, occupied.has(2) && LEAF_RING].filter(
      (r): r is number => typeof r === "number",
    );
  }, [placed]);

  // Dive out one level on Escape: the same gesture at every depth.
  useEffect(() => {
    if (focus === COMPANY_ID) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== "Escape") return;
      onFocus(graph.byId.get(focus)?.parent ?? COMPANY_ID);
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [focus, graph, onFocus]);

  // Hovering wins over the dive for what is lit, so you can read a neighbouring
  // branch without leaving the one you dived into.
  const lit = useMemo(
    () => chainOf(graph, hoverId ?? (focus === COMPANY_ID ? null : focus)),
    [graph, hoverId, focus],
  );
  const dimmed = lit.size > 0;

  const camera = useMemo(() => {
    const spot = placed.get(focus) ?? { x: CX, y: CY, ring: 0 };
    const scale = ZOOM[spot.ring] ?? 1;
    return {
      scale,
      transform: `translate(${CX - spot.x * scale} ${CY - spot.y * scale}) scale(${scale})`,
    };
  }, [placed, focus]);

  const opacityOf = (id: string) => (!dimmed || lit.has(id) ? 1 : 0.12);

  return (
    <svg
      viewBox={`0 0 ${W} ${H}`}
      className="h-full w-full select-none"
      role="img"
      aria-label={`Agent graph: ${graph.nodes.length} nodes across ${graph.hubs.length} hubs.`}
      onMouseLeave={() => setHoverId(null)}
    >
      {/* Clicking the empty field dives all the way back out. */}
      <rect width={W} height={H} fill="transparent" onClick={() => onFocus(COMPANY_ID)} />

      <g style={MOVE} transform={camera.transform}>
        {/* The rings, so the sunburst reads as rings rather than scatter. An
            empty ring is not drawn: a circle with nothing on it reads as
            something missing rather than as nothing to show. */}
        {rings.map((r) => (
          <circle
            key={r}
            cx={CX}
            cy={CY}
            r={r}
            fill="none"
            className="stroke-border"
            strokeDasharray="2 7"
            vectorEffect="non-scaling-stroke"
          />
        ))}

        {/* Edges first, so nodes always sit on top of their own spokes. */}
        {graph.nodes.map((node) => {
          if (!node.parent) return null;
          const from = placed.get(node.parent);
          const to = placed.get(node.id);
          if (!from || !to) return null;
          const both = lit.has(node.id) && lit.has(node.parent);
          return (
            <line
              key={`edge-${node.id}`}
              x1={from.x}
              y1={from.y}
              x2={to.x}
              y2={to.y}
              className={both ? BRANCH_MARK[BRANCH_OF[node.kind]] : "stroke-border"}
              stroke={both ? "currentColor" : undefined}
              strokeWidth={both ? 1.5 : 1}
              vectorEffect="non-scaling-stroke"
              style={{ ...FADE, opacity: both ? 0.85 : dimmed ? 0.08 : 0.35 }}
            />
          );
        })}

        {graph.nodes.map((node) => {
          const spot = placed.get(node.id);
          if (!spot) return null;
          return (
            <Node
              key={node.id}
              id={node.id}
              kind={node.kind}
              label={node.label}
              sub={node.sub}
              muted={node.muted}
              x={spot.x}
              y={spot.y}
              r={spot.r}
              angle={spot.angle}
              ring={spot.ring}
              scale={camera.scale}
              opacity={opacityOf(node.id)}
              // Leaf labels are noise at rest — a hundred of them overlap. They
              // appear for the branch you are reading, and for the branch you
              // have dived into.
              showLabel={spot.ring < 2 || lit.has(node.id)}
              onHover={setHoverId}
              onClick={() => onFocus(node.id === focus ? (node.parent ?? COMPANY_ID) : node.id)}
            />
          );
        })}
      </g>
    </svg>
  );
}

interface NodeProps {
  id: string;
  kind: NodeKind;
  label: string;
  sub: string;
  muted?: boolean;
  x: number;
  y: number;
  r: number;
  angle: number;
  ring: number;
  scale: number;
  opacity: number;
  showLabel: boolean;
  onHover: (id: string | null) => void;
  onClick: () => void;
}

function Node({
  id,
  kind,
  label,
  sub,
  muted,
  x,
  y,
  r,
  angle,
  ring,
  scale,
  opacity,
  showLabel,
  onHover,
  onClick,
}: NodeProps) {
  const Icon = KIND_ICON[kind];
  const hue = BRANCH_MARK[BRANCH_OF[kind]];
  const leaf = ring === 2;
  // Labels sit outside the node, away from the centre, so they never cross the
  // spokes: below on the bottom half of the dial, above on the top half.
  const below = ring === 0 || Math.sin(angle) >= 0;
  const gap = (r + 13) * scale;
  // Every node wears its icon, leaves included — at rest a leaf carries no
  // label, so the glyph is the only thing saying whether it is a card, a skill
  // or a tool.
  const iconSize = Math.min(r * 1.1, 18);

  const handle = (e: { stopPropagation: () => void }) => {
    e.stopPropagation();
    onClick();
  };

  return (
    <g
      transform={`translate(${x} ${y})`}
      style={{ ...MOVE, ...FADE, opacity: muted ? opacity * 0.6 : opacity, cursor: "pointer" }}
      role="button"
      tabIndex={0}
      aria-label={`${label} — ${sub}`}
      onClick={handle}
      onKeyDown={(e) => {
        if (e.key === "Enter" || e.key === " ") handle(e);
      }}
      onMouseEnter={() => onHover(id)}
      onFocus={() => onHover(id)}
      className={cn(hue, "focus:outline-none")}
    >
      <title>{`${label} — ${sub}`}</title>
      {/* Hubs read as hollow rings wearing their icon; leaves as solid dots.
          At 7.5px across an icon is a blob, so shape separates the two tiers
          and hue says which branch a leaf belongs to — with the legend and the
          hover label carrying the rest. */}
      <circle
        r={r}
        className={leaf ? undefined : "fill-card"}
        fill={leaf ? "currentColor" : undefined}
        stroke="currentColor"
        strokeWidth={2}
        vectorEffect="non-scaling-stroke"
      />
      {/* A generous invisible hit area: a leaf is smaller than a cursor. */}
      <circle r={Math.max(r, 14)} fill="transparent" />
      {!leaf && (
        <Icon x={-iconSize / 2} y={-iconSize / 2} width={iconSize} height={iconSize} strokeWidth={1.75} />
      )}

      {showLabel && (
        <g style={MOVE} transform={`scale(${1 / scale})`}>
          {/* Above the node the name goes on top and the sub sits between it
              and the node; below, the order flips. Either way the sub is the
              line nearest the node and neither crosses it. */}
          <text
            y={below ? gap + 4 : -gap - 13}
            textAnchor="middle"
            className="fill-foreground font-medium"
            style={{ fontSize: leaf ? 10.5 : 12 }}
          >
            {truncate(label, leaf ? 22 : 26)}
          </text>
          <text
            y={below ? gap + 17 : -gap}
            textAnchor="middle"
            className="fill-muted-foreground font-mono"
            style={{ fontSize: 9.5 }}
          >
            {sub}
          </text>
        </g>
      )}
    </g>
  );
}

function truncate(text: string, max: number): string {
  return text.length <= max ? text : `${text.slice(0, max - 1)}…`;
}
