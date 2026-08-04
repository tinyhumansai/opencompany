import { useCallback, useEffect, useMemo, useState } from "react";

import type { OpenCompanyClient } from "@/api/client";
import { listSkills, type Skill } from "@/api/skills";
import { listTasks, type Task } from "@/api/tasks";
import type { View } from "@/components/app-shell";
import { StatusPill } from "@/components/status-pill";
import type { CompanyFeed } from "@/hooks/use-company";
import type { McpServer, McpTool } from "@/lib/mcp";
import { fromDto, starterTeam, type TeamMember } from "@/lib/team";
import { AgentGraph } from "./overview/AgentGraph";
import {
  buildGraph,
  COMPANY_ID,
  countsByKind,
  withoutKinds,
  type NodeKind,
} from "./overview/graph";
import { Inspector } from "./overview/Inspector";
import { Legend } from "./overview/Legend";
import { TONE_TEXT } from "./overview/palette";
import { isOpen, stateOfWorld } from "./overview/pulse";
import { Ticker } from "./overview/Ticker";
import { tickerItems } from "./overview/pulse";

interface Props {
  feed: CompanyFeed;
  client: OpenCompanyClient;
  company: string | null;
  onNavigate: (view: View) => void;
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
 * The command centre: the whole company as one graph, filling the page.
 *
 * The company sits at the centre; its teammates, skill areas and MCP servers
 * are the hubs around it; their cards, skills and tools are the ring beyond.
 * Every edge on screen is one the host actually records — a card's assignee, a
 * skill's category, a server's advertised tools. Nothing is joined across those
 * branches, because no such edge exists to draw.
 *
 * The chrome floats over the canvas rather than boxing it in: the state line
 * top-left, the legend (which is also the lens) bottom-left, the inspector on
 * the right, and the live strip along the bottom.
 */
export function Overview({ feed, client, company, onNavigate }: Props) {
  const { status, approvals, now } = feed;

  const [sources, setSources] = useState<Sources>(EMPTY);
  const [focusId, setFocusId] = useState<string>(COMPANY_ID);
  const [hidden, setHidden] = useState<Set<NodeKind>>(() => new Set());

  // A node id from the last company means nothing in this one.
  useEffect(() => setFocusId(COMPANY_ID), [company]);

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

  const full = useMemo(
    () =>
      buildGraph({
        companyName: status.name,
        lifecycle: status.lifecycle,
        members: sources.team,
        tasks: sources.tasks,
        skills: sources.skills,
        servers: sources.servers,
        toolsByServer: sources.toolsByServer,
      }),
    [status.name, status.lifecycle, sources],
  );

  const counts = useMemo(() => countsByKind(full), [full]);
  const graph = useMemo(() => withoutKinds(full, hidden), [full, hidden]);

  const openCards = useMemo(() => sources.tasks.filter(isOpen).length, [sources.tasks]);
  const inProgress = useMemo(
    () => sources.tasks.filter((t) => t.column === "in_progress").length,
    [sources.tasks],
  );
  const enabledSkills = useMemo(() => sources.skills.filter((s) => s.enabled).length, [sources.skills]);
  const ticker = useMemo(() => tickerItems(sources.tasks, approvals), [sources.tasks, approvals]);

  const state = useMemo(
    () =>
      stateOfWorld({
        lifecycle: status.lifecycle,
        pendingApprovals: status.pending_approvals,
        openTasks: openCards,
        inProgress,
        members: sources.team.length,
        enabledSkills,
      }),
    [status, openCards, inProgress, sources.team.length, enabledSkills],
  );

  // Hiding a kind the camera is on is not an error: the graph and the inspector
  // both fall back to the company, and turning the kind back on restores the
  // focus rather than making the operator find it again.
  const toggle = useCallback((kind: NodeKind) => {
    setHidden((prev) => {
      const next = new Set(prev);
      if (next.has(kind)) next.delete(kind);
      else next.add(kind);
      return next;
    });
  }, []);

  return (
    <div className="relative flex-1 overflow-hidden">
      {/* The canvas fills the page; everything else floats over it. The inset
          keeps the sunburst clear of the inspector on a wide screen, where the
          panel is pinned rather than stacked. */}
      <div className="absolute inset-x-2 bottom-14 top-2 lg:right-[19.5rem]">
        <AgentGraph graph={graph} focusId={focusId} onFocus={setFocusId} />
      </div>

      {/* Who, and how things stand, in one line. */}
      <header className="pointer-events-none absolute left-4 top-4 max-w-[min(30rem,55%)] space-y-1.5">
        <div className="pointer-events-auto flex flex-wrap items-center gap-2">
          <h2 className="text-xl font-semibold tracking-tight">{status.name}</h2>
          <StatusPill lifecycle={status.lifecycle} />
        </div>
        <p className="flex flex-wrap items-center gap-x-2 gap-y-1 font-mono text-[11px]">
          {state.map((chip, i) => (
            <span key={chip.text} className="flex items-center gap-2">
              {i > 0 && <span className="text-border">·</span>}
              <span className={TONE_TEXT[chip.tone]}>{chip.text}</span>
            </span>
          ))}
        </p>
      </header>

      <div className="absolute bottom-16 left-4">
        <Legend counts={counts} hidden={hidden} onToggle={toggle} />
      </div>

      <div className="absolute bottom-16 right-4 top-4 flex items-start justify-end">
        <Inspector
          graph={graph}
          focusId={focusId}
          status={status}
          approvals={approvals}
          openCards={openCards}
          now={now}
          onFocus={setFocusId}
          onNavigate={onNavigate}
        />
      </div>

      <div className="absolute inset-x-4 bottom-4">
        <Ticker items={ticker} />
      </div>
    </div>
  );
}
