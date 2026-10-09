import { useEffect, useState } from "react";

import type { OpenCompanyClient } from "@/api/client";
import type { McpAgentAccess as AgentAccess, McpServer } from "@/api/types";
import { AgentAccessList } from "@/components/agent-access-list";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import { consoleHref } from "@/lib/console-paths";
import {
  MCP_PINS_INHERITED_WARNING,
  accessStateLabel,
  canToggle,
  grantedNow,
  lockedReason,
  nextTools,
} from "@/lib/mcp-scope";
import {
  partialSaveMessage,
  saveAgentAccess,
  type AgentAccessWrite,
} from "@/lib/agent-access-save";

const MEMBER_READ_ONLY =
  "Who can call a server is an admin's decision. You can see who has access.";

function toolsHref(agentId: string): string {
  return `${consoleHref("team", agentId)}?tab=tools`;
}

/**
 * Who can call one MCP server, edited in place. Every change is the teammate's
 * own `PATCH …/team/{id}` with the whole `tools` list the host computed for it.
 */
export function McpAgentAccess({
  client,
  company,
  server,
  canManage,
  onSaved,
}: {
  client: OpenCompanyClient;
  company: string | null;
  server: McpServer;
  canManage: boolean;
  onSaved: () => void;
}) {
  const [moved, setMoved] = useState<Record<string, boolean>>({});
  const [saving, setSaving] = useState(false);
  const [problem, setProblem] = useState<string | null>(null);
  const [saved, setSaved] = useState(false);

  useEffect(() => {
    setMoved({});
    setProblem(null);
    setSaved(false);
  }, [server.name]);

  const agents = server.agentAccess ?? [];
  const grant = server.accessGrant ?? `mcp:${server.name}`;
  const ticked = (agent: AgentAccess) => moved[agent.id] ?? grantedNow(agent);
  const changed = agents.filter((agent) => ticked(agent) !== grantedNow(agent));
  const pinning = changed.filter((agent) => agent.state === "inherited");
  const nameOf = (id: string) => agents.find((a) => a.id === id)?.name ?? id;

  async function save() {
    setSaving(true);
    setProblem(null);
    setSaved(false);
    const writes: AgentAccessWrite[] = [];
    for (const agent of changed) {
      const tools = nextTools(agent, ticked(agent));
      if (tools) writes.push({ agentId: agent.id, input: { tools } });
    }
    const outcome = await saveAgentAccess(client, company, writes, (agentId) =>
      setMoved((all) => {
        const rest = { ...all };
        delete rest[agentId];
        return rest;
      }),
    );
    setSaving(false);
    onSaved();
    const message = partialSaveMessage(outcome, nameOf);
    setProblem(message);
    setSaved(message === null);
  }

  if (server.agentAccess === undefined) return null;

  return (
    <section className="space-y-2" data-testid="mcp-agent-access">
      <h3 className="text-sm font-medium">Agents with access</h3>
      {agents.length === 0 ? (
        <p className="text-xs text-muted-foreground" data-testid="mcp-agent-access-empty">
          This company has no teammates yet.
        </p>
      ) : (
        <>
          {!canManage && (
            <p className="text-xs text-muted-foreground" data-testid="mcp-agent-access-read-only">
              {MEMBER_READ_ONLY}
            </p>
          )}
          <AgentAccessList
            rows={agents.map((agent) => {
              const reason = lockedReason(agent, grant);
              return {
                id: agent.id,
                name: agent.name,
                ticked: ticked(agent),
                locked: !canToggle(agent),
                holds: agent.reaches,
                holdsLabel: { yes: "Has access", no: "No access" },
                reason: reason ? (
                  <>
                    {reason}{" "}
                    <a
                      href={toolsHref(agent.id)}
                      className="underline"
                      data-testid={`mcp-access-tools-link-${agent.id}`}
                    >
                      Open Tools
                    </a>
                  </>
                ) : undefined,
                detail: (
                  <a
                    href={toolsHref(agent.id)}
                    className="text-2xs text-muted-foreground transition-opacity hover:opacity-80"
                    data-testid={`mcp-access-link-${agent.id}`}
                  >
                    {accessStateLabel(agent)}
                  </a>
                ),
              };
            })}
            team={null}
            editable={canManage}
            disabled={saving}
            listTestId="mcp-agent-access-list"
            rowTestIdPrefix="mcp-access"
            onToggle={(id, on) => {
              setSaved(false);
              setMoved((all) => ({ ...all, [id]: on }));
            }}
          />
          {!server.enabled && (
            <p className="text-xs text-muted-foreground" data-testid="mcp-agent-access-disabled">
              This server is switched off, so nobody reaches it right now. The
              list still decides who gets it when it is switched back on.
            </p>
          )}
          {pinning.length > 0 && (
            <p className="text-xs text-status-blocked-text" data-testid="mcp-agent-access-pin-warning">
              {MCP_PINS_INHERITED_WARNING}
            </p>
          )}
          {problem && (
            <Alert variant="destructive" data-testid="mcp-agent-access-problem">
              <AlertDescription>{problem}</AlertDescription>
            </Alert>
          )}
          {saved && (
            <p className="text-xs text-status-done-text" data-testid="mcp-agent-access-saved">
              Saved. Teammates pick this up on their next turn.
            </p>
          )}
          {canManage && (
            <div className="flex justify-end gap-2">
              <Button
                variant="ghost"
                size="sm"
                disabled={saving || changed.length === 0}
                onClick={() => {
                  setMoved({});
                  setProblem(null);
                }}
                data-testid="mcp-agent-access-reset"
              >
                Reset
              </Button>
              <Button
                size="sm"
                disabled={saving || changed.length === 0}
                onClick={() => void save()}
                data-testid="mcp-agent-access-save"
              >
                Save
              </Button>
            </div>
          )}
        </>
      )}
    </section>
  );
}
