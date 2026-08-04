// The company map: the whole company as one picture you can fall into.
//
// The company sits at the centre, the roster orbits it, and a teammate's own
// cards orbit them once you dive in. Diving is a zoom — the map keeps drawing
// the same scene and moves the camera onto whatever you picked — so you never
// lose the sense of where the thing you are looking at sits in the whole.
//
// Labels are counter-scaled against the zoom so they stay the same size on
// screen at every depth; only the diagram grows.

import { useEffect, useMemo } from "react";

import { cn } from "@/lib/utils";
import type { Task } from "@/api/tasks";
import { TASK_COLUMNS } from "@/lib/tasks-sample";
import { COLUMN_MARK } from "./palette";
import type { MapFocus, MapMember } from "./types";

const W = 640;
const H = 440;
const CX = W / 2;
const CY = H / 2;
/** Radius of the roster orbit, and of a teammate's own card orbit. */
const MEMBER_ORBIT = 158;
const TASK_ORBIT = 74;
/** How far in the camera pushes at each depth. */
const ZOOM = { company: 1, member: 1.55, task: 2.3 } as const;

const EASE = "cubic-bezier(0.22, 1, 0.36, 1)";
const MOVE = { transition: `transform 560ms ${EASE}` } as const;
const FADE = { transition: `opacity 360ms ${EASE}` } as const;

interface Point {
  x: number;
  y: number;
}

interface Props {
  companyName: string;
  lifecycle: string;
  members: MapMember[];
  focus: MapFocus;
  onFocus: (focus: MapFocus) => void;
}

/** Where each teammate sits: evenly spaced, starting at twelve o'clock. */
function memberPoints(count: number): Point[] {
  return Array.from({ length: count }, (_, i) => {
    const angle = -Math.PI / 2 + (i * 2 * Math.PI) / Math.max(1, count);
    return {
      x: CX + Math.cos(angle) * MEMBER_ORBIT,
      y: CY + Math.sin(angle) * MEMBER_ORBIT,
    };
  });
}

/** Where a focused teammate's cards sit: a fan around that teammate. */
function taskPoints(origin: Point, count: number): Point[] {
  return Array.from({ length: count }, (_, i) => {
    const angle = -Math.PI / 2 + (i * 2 * Math.PI) / Math.max(1, count);
    return {
      x: origin.x + Math.cos(angle) * TASK_ORBIT,
      y: origin.y + Math.sin(angle) * TASK_ORBIT,
    };
  });
}

export function CompanyMap({ companyName, lifecycle, members, focus, onFocus }: Props) {
  const points = useMemo(() => memberPoints(members.length), [members.length]);

  const openMemberIndex = indexOfFocused(members, focus);
  const openMember = openMemberIndex >= 0 ? members[openMemberIndex] : null;
  const openMemberPoint = openMemberIndex >= 0 ? points[openMemberIndex] : null;

  const cards = openMember?.tasks ?? [];
  const cardPoints = useMemo(
    () => (openMemberPoint ? taskPoints(openMemberPoint, cards.length) : []),
    [openMemberPoint, cards.length],
  );

  // Dive out one level on Escape — the same gesture at every depth.
  useEffect(() => {
    if (focus.kind === "company") return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== "Escape") return;
      onFocus(focus.kind === "task" && openMember ? { kind: "member", id: openMember.id } : { kind: "company" });
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [focus, onFocus, openMember]);

  // The camera: put the focused node under the centre of the frame, magnified.
  const scale = ZOOM[focus.kind];
  const target =
    focus.kind === "task"
      ? cardPoints[cards.findIndex((t) => t.id === focus.id)] ?? openMemberPoint ?? { x: CX, y: CY }
      : openMemberPoint ?? { x: CX, y: CY };
  const camera = `translate(${CX - target.x * scale} ${CY - target.y * scale}) scale(${scale})`;

  const dim = (on: boolean) => (on ? 1 : 0.16);

  return (
    <svg
      viewBox={`0 0 ${W} ${H}`}
      className="h-full w-full touch-none select-none"
      role="img"
      aria-label={`Company map for ${companyName}. ${members.length} teammates.`}
    >
      {/* Clicking the empty field dives back out to the whole company. */}
      <rect
        width={W}
        height={H}
        fill="transparent"
        onClick={() => onFocus({ kind: "company" })}
      />

      <g style={MOVE} transform={camera}>
        {/* The orbit the roster sits on, drawn so the ring reads as a ring. */}
        <circle
          cx={CX}
          cy={CY}
          r={MEMBER_ORBIT}
          fill="none"
          className="stroke-border"
          strokeDasharray="3 6"
          vectorEffect="non-scaling-stroke"
        />

        {points.map((p, i) => (
          <line
            key={`spoke-${members[i].id}`}
            x1={CX}
            y1={CY}
            x2={p.x}
            y2={p.y}
            className="stroke-border"
            vectorEffect="non-scaling-stroke"
            style={{ ...FADE, opacity: dim(!openMember || openMember.id === members[i].id) }}
          />
        ))}

        {/* The company itself. */}
        <Node
          x={CX}
          y={CY}
          r={40}
          scale={scale}
          label={companyName}
          sub={lifecycle}
          accent={lifecycle === "running" ? "text-[#008300]" : "text-muted-foreground"}
          pulse={lifecycle === "running"}
          onClick={() => onFocus({ kind: "company" })}
          opacity={focus.kind === "company" ? 1 : 0.45}
        />

        {members.map((m, i) => {
          const active = !openMember || openMember.id === m.id;
          return (
            <Node
              key={m.id}
              x={points[i].x}
              y={points[i].y}
              r={22 + Math.min(m.open, 6) * 2.5}
              scale={scale}
              label={m.name}
              sub={m.open > 0 ? `${m.open} open` : m.role}
              accent={m.open > 0 ? "text-[#2a78d6] dark:text-[#3987e5]" : "text-muted-foreground"}
              badge={m.open > 0 ? String(m.open) : undefined}
              opacity={dim(active)}
              onClick={() =>
                onFocus(openMember?.id === m.id ? { kind: "company" } : { kind: "member", id: m.id })
              }
            />
          );
        })}

        {/* A focused teammate's own cards, fanned around them. */}
        {openMember &&
          cards.map((task, i) => (
            <Node
              key={task.id}
              x={cardPoints[i].x}
              y={cardPoints[i].y}
              r={13}
              scale={scale}
              label={truncate(task.title, 22)}
              sub={columnLabel(task)}
              accent={COLUMN_MARK[task.column] ?? "text-muted-foreground"}
              opacity={focus.kind === "task" && focus.id !== task.id ? 0.3 : 1}
              onClick={() => onFocus({ kind: "task", id: task.id })}
            />
          ))}

        {openMember && cards.length === 0 && openMemberPoint && (
          <text
            x={openMemberPoint.x}
            y={openMemberPoint.y + 58}
            textAnchor="middle"
            className="fill-muted-foreground"
            style={{ fontSize: 11 / scale }}
          >
            nothing on their plate
          </text>
        )}
      </g>
    </svg>
  );
}

interface NodeProps {
  x: number;
  y: number;
  r: number;
  scale: number;
  label: string;
  sub: string;
  accent: string;
  badge?: string;
  pulse?: boolean;
  opacity: number;
  onClick: () => void;
}

/** One dot on the map, with its label held at a constant on-screen size. */
function Node({ x, y, r, scale, label, sub, accent, badge, pulse, opacity, onClick }: NodeProps) {
  const handle = (e: { stopPropagation: () => void }) => {
    e.stopPropagation();
    onClick();
  };
  return (
    <g
      transform={`translate(${x} ${y})`}
      style={{ ...MOVE, ...FADE, opacity, cursor: "pointer" }}
      role="button"
      tabIndex={0}
      onClick={handle}
      onKeyDown={(e) => {
        if (e.key === "Enter" || e.key === " ") handle(e);
      }}
      className="focus:outline-none"
    >
      <title>{`${label} — ${sub}`}</title>
      {pulse && (
        <circle r={r} className={cn(accent, "animate-ping")} fill="currentColor" opacity={0.12} />
      )}
      <circle
        r={r}
        className={cn(accent, "fill-card")}
        stroke="currentColor"
        strokeWidth={2}
        vectorEffect="non-scaling-stroke"
      />
      {badge && (
        <text
          textAnchor="middle"
          dominantBaseline="central"
          className={cn(accent, "fill-current font-mono font-semibold")}
          style={{ fontSize: 13 }}
        >
          {badge}
        </text>
      )}
      {/* Counter-scaled so the type stays the same size at every zoom depth. */}
      <g style={MOVE} transform={`scale(${1 / scale})`}>
        <text
          y={(r + 15) * scale}
          textAnchor="middle"
          className="fill-foreground font-medium"
          style={{ fontSize: 12 }}
        >
          {label}
        </text>
        <text
          y={(r + 15) * scale + 14}
          textAnchor="middle"
          className="fill-muted-foreground font-mono"
          style={{ fontSize: 10 }}
        >
          {sub}
        </text>
      </g>
    </g>
  );
}

/** Which teammate the current focus belongs to, or -1 at company level. */
function indexOfFocused(members: MapMember[], focus: MapFocus): number {
  if (focus.kind === "member") return members.findIndex((m) => m.id === focus.id);
  if (focus.kind === "task") {
    return members.findIndex((m) => m.tasks.some((t) => t.id === focus.id));
  }
  return -1;
}

function columnLabel(task: Task): string {
  return TASK_COLUMNS.find((c) => c.id === task.column)?.label ?? task.column;
}

function truncate(text: string, max: number): string {
  return text.length <= max ? text : `${text.slice(0, max - 1)}…`;
}
