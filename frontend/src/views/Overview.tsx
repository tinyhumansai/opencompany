import { lazy, Suspense, useEffect, useMemo, useState } from "react";

import type { OpenCompanyClient } from "@/api/client";
import { listSkills, type Skill } from "@/api/skills";
import { listTasks, type Task } from "@/api/tasks";
import type { McpServer, McpTool } from "@/lib/mcp";
import { loadMemory, type MemoryEntry } from "@/lib/memory";
import { fromDto, starterTeam, type TeamMember } from "@/lib/team";
import { adapt, buildMemoryGraph } from "./overview/kg/adapter";
import { buildKnowledgeGraph } from "./overview/kg/model";
import { ownedBy } from "./overview/pulse";

// The graph carries the force simulation and every detail card with it. Its own
// chunk means a cold load paints the frame before the physics arrives.
const KnowledgeGraph = lazy(() =>
  import("./overview/kg/KnowledgeGraph").then((m) => ({ default: m.KnowledgeGraph })),
);

interface Props {
  client: OpenCompanyClient;
  company: string | null;
}

/** Everything the graph is drawn from, fetched once per company. */
interface Sources {
  tasks: Task[];
  team: TeamMember[];
  skills: Skill[];
  servers: McpServer[];
  toolsByServer: Record<string, McpTool[]>;
}

const EMPTY: Sources = { tasks: [], team: starterTeam(), skills: [], servers: [], toolsByServer: {} };

/**
 * The command centre: the company's knowledge graph, and nothing else.
 *
 * The page is the graph — no header, no strip, no top bar (the shell hides its
 * own for this view). The company sits at the core, its departments are the
 * pillars, the jobs hang off each pillar, the teammate who does each job sits
 * above it, and their tools are the outer ring.
 *
 * Two of those rings are **derived**, not declared: a company manifest carries
 * no department field and no per-agent tool list, so `kg/adapter.ts` invents a
 * plausible structure rather than leaving the graph three rings short. See
 * `DERIVED_NOTICE` there — it is the standing caveat on this whole surface.
 */
export function Overview({ client, company }: Props) {
  const [sources, setSources] = useState<Sources>(EMPTY);

  // Memory is a local store, not a host surface (see `lib/memory.ts`), so it is
  // read straight from storage — and re-read on a company switch, since it is
  // keyed per company.
  const memories = useMemo<MemoryEntry[]>(() => loadMemory(company), [company]);

  useEffect(() => {
    let live = true;
    void (async () => {
      const [tasks, roster, skills, mcp] = await Promise.all([
        listTasks(client, company).catch(() => [] as Task[]),
        client.listTeam(company).catch(() => null),
        listSkills(client, company).catch(() => [] as Skill[]),
        client.listMcpServers(company).catch(() => ({ servers: [] as McpServer[] })),
      ]);
      if (!live) return;

      // Only a connected server advertises tools; asking a disconnected one
      // just spends a request to be told nothing.
      const connected = mcp.servers.filter((s) => s.status === "connected");
      const toolLists = await Promise.all(
        connected.map((s) =>
          client
            .listMcpTools(s.server_id, company)
            .then((r) => [s.server_id, r.tools] as const)
            .catch(() => [s.server_id, [] as McpTool[]] as const),
        ),
      );
      if (!live) return;

      setSources({
        tasks,
        team: roster?.length ? roster.map(fromDto) : starterTeam(),
        skills,
        servers: mcp.servers,
        toolsByServer: Object.fromEntries(toolLists),
      });
    })();
    return () => {
      live = false;
    };
  }, [client, company]);

  const adapted = useMemo(
    () =>
      adapt({
        members: sources.team,
        tasks: sources.tasks,
        skills: sources.skills,
        servers: sources.servers,
        toolsByServer: sources.toolsByServer,
        ownedBy,
      }),
    [sources],
  );

  const graph = useMemo(
    () => buildKnowledgeGraph(adapted.agents, adapted.departments, [], adapted.tasks),
    [adapted],
  );

  const memoryGraph = useMemo(() => buildMemoryGraph(memories), [memories]);

  return (
    // The whole viewport: the shell hides its top bar for this view, so there
    // is nothing above to subtract.
    <div className="oc-kg h-svh min-h-0 w-full min-w-0 overflow-hidden">
      <Suspense
        fallback={
          <div className="grid h-full place-items-center text-sm text-muted-foreground">
            Drawing the graph…
          </div>
        }
      >
        <KnowledgeGraph
          graph={graph}
          agents={adapted.agents}
          departments={adapted.departments}
          tasks={adapted.tasks}
          memory={memoryGraph}
        />
      </Suspense>
    </div>
  );
}
