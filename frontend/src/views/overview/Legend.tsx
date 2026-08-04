// The legend, which is also the lens.
//
// Every row names a kind, shows how many of it the graph holds, and toggles it
// off — so the same panel that tells you what a colour means is the one you use
// to cut the graph down to the part you care about. Identity never rests on
// colour alone: each row carries its icon and its name.

import { cn } from "@/lib/utils";
import { KIND_ICON } from "./AgentGraph";
import { BRANCH_MARK } from "./palette";
import { BRANCH_OF, type NodeKind } from "./graph";

/** Legend order: the chain as it reads outward from the company. */
const ORDER: { kind: NodeKind; label: string }[] = [
  { kind: "desk", label: "Teammates" },
  { kind: "card", label: "Cards" },
  { kind: "capability", label: "Skill areas" },
  { kind: "skill", label: "Skills" },
  { kind: "server", label: "MCP servers" },
  { kind: "tool", label: "Tools" },
];

interface Props {
  counts: Map<NodeKind, number>;
  hidden: Set<NodeKind>;
  onToggle: (kind: NodeKind) => void;
}

export function Legend({ counts, hidden, onToggle }: Props) {
  // A kind the host never returned is absent, not zero — an empty row would
  // read as "you have no tools" when the truth is "this host has no tool API".
  const rows = ORDER.filter((row) => (counts.get(row.kind) ?? 0) > 0);
  if (rows.length === 0) return null;

  return (
    <div className="w-52 rounded-xl border bg-card/90 p-2 backdrop-blur">
      <h3 className="px-2 pb-1.5 pt-1 font-mono text-[10px] uppercase tracking-[0.16em] text-muted-foreground">
        Legend · lens
      </h3>
      <ul>
        {rows.map(({ kind, label }) => {
          const Icon = KIND_ICON[kind];
          const off = hidden.has(kind);
          return (
            <li key={kind}>
              <button
                type="button"
                onClick={() => onToggle(kind)}
                aria-pressed={!off}
                className={cn(
                  "flex w-full items-center gap-2 rounded-lg px-2 py-1.5 text-left text-xs transition-colors hover:bg-accent/50",
                  off && "opacity-40",
                )}
              >
                <Icon className={cn("size-3.5 shrink-0", BRANCH_MARK[BRANCH_OF[kind]])} />
                <span className="min-w-0 flex-1 truncate">{label}</span>
                <span className="shrink-0 font-mono tabular-nums text-muted-foreground">
                  {counts.get(kind)}
                </span>
              </button>
            </li>
          );
        })}
      </ul>
    </div>
  );
}
