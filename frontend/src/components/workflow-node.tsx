import { Handle, type NodeProps, Position } from "@xyflow/react";

import { COLOR_CLASSES, type WorkflowNodeData } from "@/lib/workflow-sample";
import { cn } from "@/lib/utils";

/** A custom xyflow node: emoji + colored header, name, and a one-line summary. */
export function WorkflowNode({ data, selected }: NodeProps) {
  const d = data as WorkflowNodeData;
  const colors = COLOR_CLASSES[d.color];
  return (
    <div
      className={cn(
        "min-w-[180px] max-w-[240px] rounded-xl border-2 bg-card shadow-sm",
        colors.border,
        selected && "ring-2 ring-primary/40",
      )}
    >
      <Handle type="target" position={Position.Left} className="!size-2 !border-2 !bg-background" />
      <div className={cn("flex items-center gap-2 rounded-t-[10px] px-3 py-2", colors.chip)}>
        <span className="text-base leading-none" aria-hidden>
          {d.emoji}
        </span>
        <div className="min-w-0 truncate text-sm font-semibold">{d.name}</div>
      </div>
      <div className="px-3 py-2 text-[11px] leading-snug text-muted-foreground">{d.summary}</div>
      <Handle type="source" position={Position.Right} className="!size-2 !border-2 !bg-background" />
    </div>
  );
}
