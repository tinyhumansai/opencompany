import { useMemo } from "react";
import {
  Background,
  BackgroundVariant,
  Controls,
  MiniMap,
  ReactFlow,
} from "@xyflow/react";
import "@xyflow/react/dist/style.css";
import { useTheme } from "next-themes";

import { Badge } from "@/components/ui/badge";
import { WorkflowNode } from "@/components/workflow-node";
import { SAMPLE_WORKFLOW } from "@/lib/workflow-sample";

const NODE_TYPES = { oc: WorkflowNode };

/** A read-only canvas of how a company routes work, React Flow-powered. */
export function WorkflowsView() {
  const { resolvedTheme } = useTheme();
  const { nodes, edges } = useMemo(() => SAMPLE_WORKFLOW, []);

  return (
    <div className="flex flex-1 flex-col overflow-hidden">
      <div className="flex flex-wrap items-center justify-between gap-2 border-b px-4 py-3">
        <div className="flex items-center gap-2">
          <h2 className="text-sm font-semibold">Campaign pipeline</h2>
          <Badge variant="secondary">Sample</Badge>
        </div>
        <p className="text-xs text-muted-foreground">
          How this company routes a brief from intake to a published post.
        </p>
      </div>
      <div className="relative flex-1">
        <ReactFlow
          nodes={nodes}
          edges={edges}
          nodeTypes={NODE_TYPES}
          colorMode={resolvedTheme === "dark" ? "dark" : "light"}
          fitView
          fitViewOptions={{ padding: 0.2 }}
          nodesDraggable={false}
          nodesConnectable={false}
          elementsSelectable
          proOptions={{ hideAttribution: true }}
        >
          <Background variant={BackgroundVariant.Dots} gap={20} size={1} />
          <Controls showInteractive={false} />
          <MiniMap pannable zoomable className="!hidden sm:!block" />
        </ReactFlow>
      </div>
    </div>
  );
}
