import { AlertTriangle, ArrowLeft, Unplug } from "lucide-react";

import type { OpenCompanyClient } from "@/api/client";
import type { McpHealth, McpServer } from "@/api/types";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Separator } from "@/components/ui/separator";
import {
  connectedOn,
  mcpProviderSlug,
  mcpStanding,
  probedOn,
} from "@/lib/connection-detail";
import type { McpBridgeState } from "@/lib/mcp-bridge";
import {
  UsageSection,
  useConnectionUsage,
} from "@/views/connections/connection-usage";
import { mcpProvenanceNote, mcpRemovalNote } from "@/lib/mcp-registry";
import { McpToolPermissions } from "@/views/mcp/McpToolPermissions";

const PROVENANCE_LABELS: Record<string, string> = {
  manifest: "manifest",
  registry: "directory",
  runtime: "console",
  default: "built in",
};

interface Props {
  client: OpenCompanyClient;
  company: string | null;
  server: McpServer;
  health: McpHealth | undefined;
  canManage: boolean;
  /** Whether the agent-side MCP bridge is compiled into this host (issue #567). */
  bridge: McpBridgeState;
  /** Bumped when a probe re-ran, so the permissions read is not stale. */
  reloadKey: number;
  /** Scroll the permissions panel into view once it has something to show. */
  focusPermissions: boolean;
  /** Absent when this row cannot be removed from the console. */
  onDisconnect: (() => void) | null;
  onBack: () => void;
}

/**
 * One MCP server, as a page: what it is, who can reach it, and what each of
 * its tools is allowed to do.
 */
export function McpServerPage({
  client,
  company,
  server,
  health,
  canManage,
  bridge,
  reloadKey,
  focusPermissions,
  onDisconnect,
  onBack,
}: Props) {
  const standing = mcpStanding(server, health);
  const probedAt = probedOn(health?.checkedAtMillis);
  const usage = useConnectionUsage(
    client,
    company,
    mcpProviderSlug(server.name),
  );

  return (
    <div className="space-y-4" data-testid="mcp-server-page">
      <div className="flex flex-wrap items-center gap-3">
        <Button
          size="sm"
          variant="ghost"
          onClick={onBack}
          className="-ml-2 gap-1 text-muted-foreground"
          data-testid="mcp-page-back"
        >
          <ArrowLeft className="size-4" />
          MCP Servers
        </Button>
        <h2 className="text-base font-semibold">{server.name}</h2>
        <Badge
          variant="outline"
          className="font-normal"
          data-testid="mcp-page-provenance"
        >
          {PROVENANCE_LABELS[server.source] ?? server.source}
        </Badge>
        <span
          className="text-xs text-muted-foreground"
          data-testid="mcp-page-standing"
        >
          {standing.summary}
        </span>
        {onDisconnect && canManage && (
          <Button
            size="sm"
            variant="outline"
            onClick={onDisconnect}
            className="ml-auto gap-1"
            data-testid="mcp-page-disconnect"
          >
            <Unplug className="size-3.5" />
            Disconnect
          </Button>
        )}
      </div>

      <Separator />

      <div className="flex flex-wrap items-start justify-between gap-2">
        <div className="space-y-1">
          <p className="font-mono text-xs break-all text-muted-foreground">
            {server.endpoint}
          </p>
          <p
            className="text-xs text-muted-foreground"
            data-testid="mcp-page-probe"
          >
            {standing.probe}
            {probedAt !== null && ` · ${probedAt}`}
          </p>
          <p
            className="text-xs text-muted-foreground"
            data-testid="mcp-page-connected-on"
          >
            {connectedOn(undefined)} —{" "}
            {server.source === "registry"
              ? "this host does connect a directory install, but records no date for it."
              : "MCP has no connect step to record one."}
          </p>
          {health && health.status !== "ok" && health.message && (
            <p className="text-xs text-muted-foreground">{health.message}</p>
          )}
        </div>
      </div>

      {bridge !== "absent" &&
        standing.live &&
        server.reachableBy !== undefined && (
        <div className="flex flex-wrap items-center justify-between gap-2">
          {server.reachableBy.length === 0 ? (
            <p
              className="flex items-start gap-2 rounded-md border border-destructive/30 bg-destructive/10 px-2 py-1 text-xs font-medium text-destructive"
              data-testid="mcp-page-reachability"
            >
              <AlertTriangle className="mt-0.5 size-3.5 shrink-0" />
              <span>
                No agent can reach this server — no tool grant covers{" "}
                <code className="font-mono">mcp:{server.name}</code>.
              </span>
            </p>
          ) : (
            <p
              className="text-xs text-muted-foreground"
              data-testid="mcp-page-reachability"
            >
              Reachable by:{" "}
              <span className="font-medium text-foreground">
                {server.reachableBy.map((agent) => agent.name).join(", ")}
              </span>
            </p>
          )}
          <a
            href="#/company"
            className="text-xs font-medium text-muted-foreground underline"
            data-testid="mcp-page-edit-agents"
          >
            Edit agents
          </a>
        </div>
      )}

      {!standing.live && (
        <p className="flex items-start gap-2 rounded-md bg-muted/40 p-2 text-xs text-muted-foreground">
          <AlertTriangle className="mt-px size-3 shrink-0" />
          <span>
            This server is turned off, so no agent receives its tools whatever
            their grants say and whatever the endpoint answers. Its
            configuration and any stored credential survive — turning it back on
            restores its tools on the next turn.
          </span>
        </p>
      )}

      <Separator />

      <McpToolPermissions
        client={client}
        company={company}
        server={server}
        canManage={canManage}
        reloadKey={reloadKey}
        focus={focusPermissions}
      />

      <Separator />

      <UsageSection
        usage={usage}
        perConnection={`Successful tool calls your agents made through ${server.name}, counted under mcp:${server.name.trim().toLowerCase()} so a Composio provider of the same name cannot be read as this one.`}
      />

      <Separator />

      <section className="space-y-1" aria-label="Removing this server">
        <h4 className="text-xs font-medium tracking-wide text-muted-foreground uppercase">
          What a disconnect reaches
        </h4>
        <p
          className="text-xs text-muted-foreground"
          data-testid="mcp-page-disconnect-scope"
        >
          {mcpRemovalNote(server.source)} Nothing is revoked at the
          server&apos;s own end: no token it issued is invalidated and no
          session there is closed. Revoke those where they were issued.
        </p>
        <p className="text-xs text-muted-foreground">
          {mcpProvenanceNote(server.source)}
        </p>
      </section>
    </div>
  );
}
