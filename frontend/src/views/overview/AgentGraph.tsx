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
  Brain,
  Building2,
  Circle,
  Sparkles,
  SquareKanban,
  UserRound,
  Wrench,
  type LucideIcon,
} from "lucide-react";

import type { MemoryEntry } from "@/lib/memory";
import { cn } from "@/lib/utils";
import { BRANCH_MARK } from "./palette";
import {
  BRANCH_OF,
  chainOf,
  COMPANY_ID,
  layoutGraph,
  type Graph,
  type NodeKind,
  type Placed,
} from "./graph";
import { CORE_OPEN, CORE_REST, hexPoints, layoutMemory } from "./memory-core";

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
  memory: Brain,
  desk: UserRound,
  card: SquareKanban,
  capability: Sparkles,
  skill: Circle,
  server: Blocks,
  tool: Wrench,
};

/**
 * Slow wander and a luminescent breathe for the core.
 *
 * One drift and one breathe per **layer**, not per node — memories are
 * partitioned across three of them by hash. A company with a few hundred
 * memories would otherwise mean a few hundred independent SVG animations;
 * three barely register. The stir only runs while the pointer is over the
 * core, so the field comes alive when you look at it and is still otherwise.
 */
const GRAPH_STYLE = `
@keyframes oc-core-drift { from { transform: translate(0,0); } to { transform: translate(var(--oc-dx), var(--oc-dy)); } }
@keyframes oc-core-breathe { from { opacity: 0.72; } to { opacity: 1; } }
@keyframes oc-core-stir { from { transform: translate(0,0); } to { transform: translate(var(--oc-sx), var(--oc-sy)); } }
.oc-core-layer { animation: oc-core-drift 21s ease-in-out infinite alternate, oc-core-breathe 7s ease-in-out infinite alternate; }
.oc-core-layer-1 { animation-delay: -8s, -3s; }
.oc-core-layer-2 { animation-delay: -15s, -6s; }
.oc-core-stir { animation: oc-core-stir 2s ease-in-out infinite alternate; animation-play-state: paused; }
.oc-core:hover .oc-core-stir { animation-play-state: running; }

/* The wash of light behind the constellation swells on its own slow cycle. */
@keyframes oc-glow { from { opacity: 0.05; } to { opacity: 0.14; } }
.oc-glow { animation: oc-glow 7s ease-in-out infinite alternate; }

/* Spokes read as pathways out of the core: a slow outward dash flow. */
@keyframes oc-ray { to { stroke-dashoffset: -10; } }
.oc-ray { stroke-dasharray: 5 5; animation: oc-ray 2.4s linear infinite; }

/* The backdrop turns, slowly enough that you notice it only if you wait. */
@keyframes oc-spin { to { transform: rotate(360deg); } }
.oc-orbit { transform-origin: ${W / 2}px ${H / 2}px; animation: oc-spin 260s linear infinite; }
.oc-orbit-back { animation-direction: reverse; animation-duration: 380s; }
@keyframes oc-grid-drift { from { transform: translate(0,0); } to { transform: translate(28px, 18px); } }
.oc-grid { animation: oc-grid-drift 90s linear infinite alternate; }

@media (prefers-reduced-motion: reduce) {
  .oc-core-layer, .oc-core-stir, .oc-glow, .oc-ray, .oc-orbit, .oc-grid { animation: none; }
}
`;

/** Faint, slowly-turning rings well outside the graph. Pure atmosphere. */
const ORBITS = [372, 344, 316];

/** How many sparks ride the spokes at once. */
const SPARK_COUNT = 14;

/**
 * Whether the viewer asked for less motion.
 *
 * The CSS animations honour `prefers-reduced-motion` through a media query, but
 * the sparks are SMIL — no media query reaches them — so they are not rendered
 * at all rather than animated and ignored.
 */
function useReducedMotion(): boolean {
  const [reduced, setReduced] = useState(
    () => typeof window !== "undefined" && window.matchMedia("(prefers-reduced-motion: reduce)").matches,
  );
  useEffect(() => {
    const query = window.matchMedia("(prefers-reduced-motion: reduce)");
    const onChange = () => setReduced(query.matches);
    query.addEventListener("change", onChange);
    return () => query.removeEventListener("change", onChange);
  }, []);
  return reduced;
}

/** Per-layer drift and stir vectors. Three layers keeps the field incoherent. */
const LAYERS = [
  { "--oc-dx": "1.1px", "--oc-dy": "-0.8px", "--oc-sx": "2.6px", "--oc-sy": "1.8px" },
  { "--oc-dx": "-0.9px", "--oc-dy": "1.2px", "--oc-sx": "-2.2px", "--oc-sy": "2.4px" },
  { "--oc-dx": "0.7px", "--oc-dy": "1px", "--oc-sx": "1.9px", "--oc-sy": "-2.5px" },
] as unknown as React.CSSProperties[];

interface Props {
  graph: Graph;
  /** The node the camera is on, or the company when nothing is dived into. */
  focusId: string;
  /** Whether the memory core is bloomed open. */
  coreOpen: boolean;
  onFocus: (id: string) => void;
  onToggleCore: () => void;
}

export function AgentGraph({ graph, focusId, coreOpen, onFocus, onToggleCore }: Props) {
  const [hoverId, setHoverId] = useState<string | null>(null);

  const placed = useMemo(
    () => layoutGraph(graph, { cx: CX, cy: CY, hubRing: HUB_RING, leafRing: LEAF_RING }),
    [graph],
  );

  const memories = useMemo(
    () => graph.nodes.filter((n) => n.kind === "memory"),
    [graph],
  );
  const constellation = useMemo(
    () => layoutMemory(memories.map((n) => n.payload as MemoryEntry), LAYERS.length),
    [memories],
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

  // A focused memory keeps the camera on the company: the bloom is already the
  // zoom, and moving the camera as well would magnify the core twice.
  const memoryFocus = graph.byId.get(focusId)?.kind === "memory" ? focusId : null;

  // Dive out one level on Escape: the same gesture at every depth. Inside the
  // core that means memory → core → closed, so the key never skips a step.
  useEffect(() => {
    if (focus === COMPANY_ID && !coreOpen && !memoryFocus) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== "Escape") return;
      if (memoryFocus) onFocus(COMPANY_ID);
      else if (coreOpen) onToggleCore();
      else onFocus(graph.byId.get(focus)?.parent ?? COMPANY_ID);
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [focus, coreOpen, memoryFocus, graph, onFocus, onToggleCore]);

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

  // With the core open the sunburst recedes almost entirely — it is still there
  // for context, but the operator is reading the middle, not the ring.
  const recede = coreOpen ? 0.18 : 1;
  const reduced = useReducedMotion();

  // Which spokes the sparks ride. Spread deterministically across the edge
  // list — a stride rather than a sample — so every branch gets some traffic
  // and the picture never reshuffles between renders.
  const sparks = useMemo(() => {
    const edges = graph.nodes
      .filter((n) => n.parent && n.kind !== "memory")
      .map((n) => ({ from: placed.get(n.parent!), to: placed.get(n.id) }))
      .filter((e): e is { from: Placed; to: Placed } => !!e.from && !!e.to);
    if (edges.length === 0) return [];

    const stride = Math.max(1, Math.floor(edges.length / SPARK_COUNT));
    return Array.from({ length: Math.min(SPARK_COUNT, edges.length) }, (_, i) => {
      const { from, to } = edges[(i * stride) % edges.length];
      return {
        path: `M${from.x.toFixed(1)},${from.y.toFixed(1)} L${to.x.toFixed(1)},${to.y.toFixed(1)}`,
        duration: 2.6 + (i % 5) * 0.4,
        delay: (i * 0.41) % 3,
      };
    });
  }, [graph, placed]);

  const opacityOf = (id: string) =>
    id === COMPANY_ID ? (!dimmed || lit.has(id) ? 1 : 0.12) : (!dimmed || lit.has(id) ? 1 : 0.12) * recede;

  return (
    <svg
      viewBox={`0 0 ${W} ${H}`}
      className="h-full w-full select-none"
      role="img"
      aria-label={`Agent graph: ${graph.nodes.length} nodes across ${graph.hubs.length} hubs.`}
      onMouseLeave={() => setHoverId(null)}
    >
      <style dangerouslySetInnerHTML={{ __html: GRAPH_STYLE }} />
      <defs>
        <pattern id="oc-grid" width="34" height="34" patternUnits="userSpaceOnUse">
          <path d="M 34 0 L 0 0 0 34" fill="none" className="stroke-border" strokeWidth="0.5" />
        </pattern>
        <radialGradient id="oc-core-glow">
          <stop offset="0%" stopColor="currentColor" stopOpacity="0.9" />
          <stop offset="100%" stopColor="currentColor" stopOpacity="0" />
        </radialGradient>
      </defs>

      {/* Clicking the empty field dives all the way back out. */}
      <rect width={W} height={H} fill="transparent" onClick={() => onFocus(COMPANY_ID)} />

      {/* Backdrop: a faint drifting grid and slowly-turning orbital rings.
          Decoration, and deliberately so — it gives the canvas depth without
          encoding anything, so nothing here can be misread as data. */}
      <g className="pointer-events-none text-muted-foreground" aria-hidden>
        <g className="oc-grid">
          <rect x={-40} y={-40} width={W + 80} height={H + 80} fill="url(#oc-grid)" opacity={0.3} />
        </g>
        {ORBITS.map((r, i) => (
          <circle
            key={r}
            className={cn("oc-orbit", i % 2 === 1 && "oc-orbit-back")}
            cx={CX}
            cy={CY}
            r={r}
            fill="none"
            stroke="currentColor"
            strokeWidth={0.75}
            strokeDasharray={i === 1 ? "1 12" : "2 18"}
            opacity={0.2}
          />
        ))}
      </g>

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

        {/* Edges first, so nodes always sit on top of their own spokes. A
            memory has no spoke: it sits inside the company, not beside it. */}
        {graph.nodes.map((node) => {
          if (!node.parent || node.kind === "memory") return null;
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
              className={cn(both ? BRANCH_MARK[BRANCH_OF[node.kind]] : "stroke-border", "oc-ray")}
              stroke={both ? "currentColor" : undefined}
              strokeWidth={both ? 1.5 : 1}
              vectorEffect="non-scaling-stroke"
              style={{ ...FADE, opacity: (both ? 0.85 : dimmed ? 0.08 : 0.35) * recede }}
            />
          );
        })}

        {/* Synapse sparks: pulses riding the spokes, so the company reads as
            something running rather than something drawn. They carry no data —
            which spoke a spark is on means nothing — so they stay dim enough
            never to compete with the marks that do. */}
        {!reduced && (
          <g className="pointer-events-none text-foreground" aria-hidden opacity={0.5 * recede}>
            {sparks.map((spark, i) => (
              <circle key={i} r={2} fill="currentColor">
                <animateMotion
                  path={spark.path}
                  dur={`${spark.duration}s`}
                  begin={`${spark.delay}s`}
                  repeatCount="indefinite"
                />
                <animate
                  attributeName="opacity"
                  values="0;1;1;0"
                  keyTimes="0;0.15;0.85;1"
                  dur={`${spark.duration}s`}
                  begin={`${spark.delay}s`}
                  repeatCount="indefinite"
                />
              </circle>
            ))}
          </g>
        )}

        {graph.nodes.map((node) => {
          // The company is drawn as its core, below; memories live inside it.
          if (node.kind === "company" || node.kind === "memory") return null;
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

        <MemoryCore
          nodes={memories}
          spots={constellation}
          companyLabel={graph.byId.get(COMPANY_ID)?.label ?? ""}
          companySub={graph.byId.get(COMPANY_ID)?.sub ?? ""}
          open={coreOpen}
          focusId={memoryFocus}
          scale={camera.scale}
          opacity={opacityOf(COMPANY_ID)}
          onOpen={onToggleCore}
          onFocus={onFocus}
        />
      </g>
    </svg>
  );
}

interface CoreProps {
  nodes: Graph["nodes"];
  spots: Map<string, { x: number; y: number; r: number; layer: number }>;
  companyLabel: string;
  companySub: string;
  open: boolean;
  focusId: string | null;
  scale: number;
  opacity: number;
  onOpen: () => void;
  onFocus: (id: string) => void;
}

/**
 * The company, drawn as what it knows.
 *
 * At rest it is a small disc of memories — the centre of the graph shows the
 * company's knowledge rather than an opaque dot standing in for it. Opening it
 * blooms the disc through one group transform, so the constellation is not
 * re-laid-out, just brought closer.
 */
function MemoryCore({
  nodes,
  spots,
  companyLabel,
  companySub,
  open,
  focusId,
  scale,
  opacity,
  onOpen,
  onFocus,
}: CoreProps) {
  const bloom = (open ? CORE_OPEN : CORE_REST) / CORE_REST;
  const discR = open ? CORE_OPEN : CORE_REST;
  const labelGap = (discR + 16) * scale;

  return (
    <g className="oc-core" style={{ ...FADE, opacity }}>
      {/* A wash of light behind the field, breathing on its own slow cycle. */}
      <circle
        className="oc-glow pointer-events-none text-foreground"
        cx={CX}
        cy={CY}
        r={discR * 1.6}
        fill="url(#oc-core-glow)"
        style={MOVE}
        aria-hidden
      />

      {/* The rim, and the hit area that opens and closes the core. A recessive
          disc, not a solid one: the constellation inside is the thing to read,
          and a bright fill would flatten it against the rim. */}
      <circle
        cx={CX}
        cy={CY}
        r={discR}
        className="fill-muted/40 stroke-border"
        strokeWidth={1.5}
        vectorEffect="non-scaling-stroke"
        style={{ ...MOVE, cursor: "pointer" }}
        role="button"
        tabIndex={0}
        aria-label={`${companyLabel} — ${nodes.length} memories. ${open ? "Close" : "Open"} the memory core.`}
        onClick={(e) => {
          e.stopPropagation();
          onOpen();
        }}
        onKeyDown={(e) => {
          if (e.key === "Enter" || e.key === " ") onOpen();
        }}
      />

      {/* An empty core says so, rather than showing a blank disc. */}
      {nodes.length === 0 && (
        <text
          x={CX}
          y={CY + 4}
          textAnchor="middle"
          className="fill-muted-foreground font-mono"
          style={{ fontSize: 10 / scale }}
        >
          no memories yet
        </text>
      )}

      <g style={MOVE} transform={`translate(${CX} ${CY}) scale(${bloom})`}>
        {LAYERS.map((vars, layer) => (
          <g key={layer} className={`oc-core-layer oc-core-layer-${layer}`} style={vars}>
            <g className="oc-core-stir" style={vars}>
              {nodes.map((node) => {
                const spot = spots.get(memoryIdOf(node.id));
                if (!spot || spot.layer !== layer) return null;
                const focused = focusId === node.id;
                return (
                  <g
                    key={node.id}
                    transform={`translate(${spot.x} ${spot.y})`}
                    role="button"
                    tabIndex={open ? 0 : -1}
                    aria-label={`${node.label} — ${node.sub}`}
                    style={{ cursor: "pointer" }}
                    onClick={(e) => {
                      e.stopPropagation();
                      // Reading one memory implies opening the core it sits in.
                      if (!open) onOpen();
                      onFocus(node.id);
                    }}
                    onKeyDown={(e) => {
                      if (e.key === "Enter" || e.key === " ") onFocus(node.id);
                    }}
                  >
                    <title>{`${node.label} — ${node.sub}`}</title>
                    <polygon
                      points={hexPoints(spot.r)}
                      className="fill-foreground"
                      opacity={focused ? 1 : 0.75}
                    />
                    {focused && (
                      <circle
                        r={spot.r + 3}
                        fill="none"
                        className="stroke-foreground"
                        strokeWidth={1.5}
                        vectorEffect="non-scaling-stroke"
                      />
                    )}
                    {/* A hexagon is 2–5px across; the cursor is bigger. */}
                    <circle r={Math.max(spot.r, 7)} fill="transparent" />
                    {open && (
                      <text
                        y={-spot.r - 4}
                        textAnchor="middle"
                        className="fill-muted-foreground"
                        style={{ fontSize: 6.5 / bloom }}
                      >
                        {truncate(node.label, 24)}
                      </text>
                    )}
                  </g>
                );
              })}
            </g>
          </g>
        ))}
      </g>

      <g style={MOVE} transform={`translate(${CX} ${CY}) scale(${1 / scale})`}>
        <text
          y={labelGap}
          textAnchor="middle"
          className="fill-foreground font-semibold"
          style={{ fontSize: 13 }}
        >
          {companyLabel}
        </text>
        <text
          y={labelGap + 15}
          textAnchor="middle"
          className="fill-muted-foreground font-mono"
          style={{ fontSize: 10 }}
        >
          {nodes.length > 0 ? `${companySub} · ${nodes.length} memories` : companySub}
        </text>
      </g>
    </g>
  );
}

/** `memory:<entry id>` → the entry id the constellation is keyed by. */
function memoryIdOf(nodeId: string): string {
  return nodeId.slice("memory:".length);
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
  const iconSize = Math.min(r * 1.15, 18);

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
      {/* Every node is a ring wearing its icon — the tiers differ by size and
          stroke weight, not by becoming a different thing. Shape says what a
          node is, hue says which branch it belongs to, and the legend names
          both, so identity never rests on colour. */}
      <circle
        r={r}
        className="fill-card"
        stroke="currentColor"
        strokeWidth={2}
        vectorEffect="non-scaling-stroke"
      />
      {/* A generous invisible hit area: a leaf is smaller than a cursor. */}
      <circle r={Math.max(r, 14)} fill="transparent" />
      <Icon
        x={-iconSize / 2}
        y={-iconSize / 2}
        width={iconSize}
        height={iconSize}
        strokeWidth={leaf ? 2.4 : 1.75}
      />

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
