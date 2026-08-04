// The memory core: the constellation of what the company remembers, drawn as
// the centre of the graph rather than as one opaque dot.
//
// Positions are deterministic — a golden-angle sunflower inside each kind's
// cluster — so the constellation never reshuffles between renders, and the
// layout can be asserted without a simulation, a clock, or a random seed.

import type { MemoryEntry, MemoryKind } from "@/lib/memory";

/** The disc's radius in graph units at rest. Kept well inside the hub ring. */
export const CORE_REST = 58;

/** The disc's radius once the core is opened. */
export const CORE_OPEN = 134;

/**
 * The golden angle. Successive points at this turn never line up into spokes,
 * which is what makes a sunflower spread read as an even scatter rather than a
 * spiral.
 */
const GOLDEN_ANGLE = 2.399963229728653;

/** How far a kind's cluster sits from the centre, as a fraction of the disc. */
const CLUSTER_ORBIT = 0.44;

/** How far a cluster's own points spread, as a fraction of the disc. */
const CLUSTER_SPREAD = 0.34;

export interface MemorySpot {
  x: number;
  y: number;
  r: number;
  /** Which of the drift layers this node rides. */
  layer: number;
}

/**
 * Where each memory sits inside the disc.
 *
 * Entries cluster by kind — preferences with preferences, people with people —
 * so the core reads as structured rather than as noise, and a cluster growing
 * is visible at a glance. Coordinates are in graph units around `(0, 0)`; the
 * caller translates and scales the whole group, so blooming the core is one
 * transform rather than a re-layout.
 */
export function layoutMemory(entries: MemoryEntry[], layers: number): Map<string, MemorySpot> {
  const spots = new Map<string, MemorySpot>();
  const byKind = new Map<MemoryKind, MemoryEntry[]>();
  for (const entry of entries) {
    const list = byKind.get(entry.kind);
    if (list) list.push(entry);
    else byKind.set(entry.kind, [entry]);
  }

  const clusters = [...byKind.entries()];
  clusters.forEach(([, group], k) => {
    // One cluster sits dead centre; several ring the middle evenly.
    const anchorAngle = -Math.PI / 2 + (k * 2 * Math.PI) / clusters.length;
    const anchorR = clusters.length === 1 ? 0 : CLUSTER_ORBIT * CORE_REST;
    const ax = Math.cos(anchorAngle) * anchorR;
    const ay = Math.sin(anchorAngle) * anchorR;

    group.forEach((entry, i) => {
      // sqrt spacing keeps the density even out to the cluster's edge; a
      // linear radius would pile everything at the centre.
      const spread = CLUSTER_SPREAD * CORE_REST * Math.sqrt((i + 0.5) / group.length);
      const angle = i * GOLDEN_ANGLE + k;
      const x = ax + Math.cos(angle) * spread;
      const y = ay + Math.sin(angle) * spread;
      spots.set(entry.id, {
        ...clampToDisc(x, y, CORE_REST * 0.94),
        r: nodeRadius(entry),
        layer: hash(entry.id) % Math.max(1, layers),
      });
    });
  });

  return spots;
}

/**
 * How big one memory reads: by how much of it there is to remember.
 *
 * Clamped hard at both ends — a one-line fact still has to be clickable, and a
 * long note must not swallow its neighbours.
 */
export function nodeRadius(entry: MemoryEntry): number {
  return 2 + Math.min(3, entry.body.length / 220);
}

/**
 * A pointy-top hexagon's points, so memories are visibly not the round nodes
 * of the sunburst around them. Cached: radii repeat heavily across the field.
 */
const HEX_CACHE = new Map<number, string>();
export function hexPoints(r: number): string {
  const key = Math.round(r * 100);
  const cached = HEX_CACHE.get(key);
  if (cached) return cached;
  const points = Array.from({ length: 6 }, (_, k) => {
    const a = (k * Math.PI) / 3 - Math.PI / 2;
    return `${(r * Math.cos(a)).toFixed(2)},${(r * Math.sin(a)).toFixed(2)}`;
  }).join(" ");
  HEX_CACHE.set(key, points);
  return points;
}

function clampToDisc(x: number, y: number, max: number): { x: number; y: number } {
  const d = Math.hypot(x, y);
  if (d <= max || d === 0) return { x, y };
  return { x: (x / d) * max, y: (y / d) * max };
}

function hash(s: string): number {
  let h = 0;
  for (const ch of s) h = (h * 31 + ch.charCodeAt(0)) >>> 0;
  return h;
}
