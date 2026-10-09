import { useState } from "react";
import {
  AlertTriangle,
  ArrowLeft,
  Info,
  KeyRound,
  Loader2,
  LogIn,
  Plug,
  Unplug,
} from "lucide-react";

import type { OpenCompanyClient } from "@/api/client";
import { updateMcpServer } from "@/api/mcp";
import { ApiError, type McpHealth, type McpServer, type RosterAgent } from "@/api/types";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Textarea } from "@/components/ui/textarea";
import {
  connectedOn,
  mcpProviderSlug,
  mcpStanding,
  probedOn,
} from "@/lib/connection-detail";
import { mcpHealthBadge, type McpBridgeState } from "@/lib/mcp-bridge";
import {
  UsageSection,
  useConnectionUsage,
} from "@/views/connections/connection-usage";
import {
  mcpDisplayName,
  mcpProvenanceNote,
  mcpRemovalNote,
  REGISTRY_OAUTH_UNSUPPORTED_NOTICE,
  registryOauthUnsupported,
} from "@/lib/mcp-registry";
import {
  McpServerIcon,
  type PrimaryAction,
} from "@/views/connections/McpServerTable";
import { McpAgentAccess } from "@/views/mcp/McpAgentAccess";
import { McpToolPermissions } from "@/views/mcp/McpToolPermissions";

const PROVENANCE_LABELS: Record<string, string> = {
  manifest: "manifest",
  registry: "directory",
  runtime: "custom",
  default: "built in",
};

const PRIMARY = {
  sign_in: { text: "Sign in", Icon: LogIn },
  add_token: { text: "Add credential", Icon: KeyRound },
  rotate_env: { text: "Add credentials", Icon: KeyRound },
  connect: { text: "Connect", Icon: Plug },
} as const;

interface Props {
  client: OpenCompanyClient;
  company: string | null;
  server: McpServer;
  health: McpHealth | undefined;
  canManage: boolean;
  bridge: McpBridgeState;
  /** Whether a `needs_approval` mode parks a call on this host; `undefined` if unknown. */
  approvalsPark?: boolean;
  /** The roster, for the per-teammate lens on this server's tool permissions. */
  agents?: RosterAgent[];
  /** Bumped when a probe re-ran, so the permissions read is not stale. */
  reloadKey: number;
  /** Scroll the permissions panel into view once it has something to show. */
  focusPermissions: boolean;
  /** What this server needs to connect, if anything. */
  primary: PrimaryAction;
  busy: string | null;
  onPrimary: () => void;
  /** Absent when this row cannot be removed from the console. */
  onDisconnect: (() => void) | null;
  onBack: () => void;
  /** Re-read the server list after an access change. */
  onAccessSaved: () => void;
}

/** One MCP server: what it is, who can reach it, and what its tools may do. */
export function McpServerPage({
  client,
  company,
  server,
  health,
  canManage,
  bridge,
  approvalsPark,
  agents = [],
  reloadKey,
  focusPermissions,
  primary,
  busy,
  onPrimary,
  onDisconnect,
  onBack,
  onAccessSaved,
}: Props) {
  const [detailsOpen, setDetailsOpen] = useState(false);
  const standing = mcpStanding(server, health);
  const badge = mcpHealthBadge(health, server.authConfigured, bridge);
  const usage = useConnectionUsage(client, company, mcpProviderSlug(server.name));
  const action = primary && canManage ? PRIMARY[primary.kind] : null;
  const reach = server.reachableBy;

  return (
    <div
      className="mx-auto w-full max-w-3xl space-y-6"
      data-testid="mcp-server-page"
      onMouseDown={(event) => {
        const target = event.target as HTMLElement | null;
        if (event.detail > 1 && !target?.closest(".select-text, input, textarea")) {
          event.preventDefault();
        }
      }}
    >
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

      <header className="flex flex-col gap-4 sm:flex-row sm:items-start">
        <div className="flex min-w-0 flex-1 items-start gap-4">
          <McpServerIcon
            iconUrl={server.iconUrl}
            name={mcpDisplayName(server)}
            className="size-14"
          />
          <div className="min-w-0 space-y-1">
            <div className="flex flex-wrap items-center gap-2">
              <h2 className="truncate text-xl font-semibold">{mcpDisplayName(server)}</h2>
              <Badge variant="outline" className="font-normal" data-testid="mcp-page-provenance">
                {PROVENANCE_LABELS[server.source] ?? server.source}
              </Badge>
            </div>
            <p className="flex flex-wrap items-center gap-x-2 text-xs text-muted-foreground">
              {badge && (
                <span
                  className={
                    badge.tone === "delivering"
                      ? "font-medium text-status-done-text"
                      : badge.tone === "configured"
                        ? "font-medium"
                        : "font-medium text-status-blocked-text"
                  }
                >
                  {badge.label}
                </span>
              )}
              <span data-testid="mcp-page-standing">{standing.summary}</span>
            </p>
          </div>
        </div>
        <div className="flex items-center gap-2 sm:shrink-0">
          <Button
            variant="outline"
            size="icon-sm"
            aria-label="Connection details"
            title="Connection details"
            data-testid="mcp-page-details-open"
            onClick={() => setDetailsOpen(true)}
          >
            <Info className="size-4" />
          </Button>
          {onDisconnect && canManage && (
            <Button
              size="sm"
              variant="outline"
              onClick={onDisconnect}
              className="flex-1 gap-1 sm:flex-none"
              data-testid="mcp-page-disconnect"
            >
              <Unplug className="size-3.5" />
              Disconnect
            </Button>
          )}
          {action && (
            <Button
              size="sm"
              className="flex-1 sm:flex-none"
              disabled={busy !== null}
              data-testid="mcp-page-primary"
              onClick={onPrimary}
            >
              {busy === server.name ? (
                <Loader2 className="size-3.5 animate-spin" />
              ) : (
                <action.Icon className="size-3.5" />
              )}
              {action.text}
            </Button>
          )}
        </div>
      </header>

      <McpDescription client={client} company={company} server={server} canManage={canManage} />

      {health && health.status !== "ok" && health.message.trim() && (
        <p className="flex items-start gap-2 rounded-md border border-status-blocked-text/30 bg-status-blocked-text/10 px-3 py-2 text-xs text-status-blocked-text">
          <AlertTriangle className="mt-0.5 size-3.5 shrink-0" />
          <span>{health.message}</span>
        </p>
      )}

      {registryOauthUnsupported(server, health) && (
        <p className="text-xs text-muted-foreground" data-testid="mcp-no-credential-control">
          {REGISTRY_OAUTH_UNSUPPORTED_NOTICE}
        </p>
      )}

      {!standing.live && (
        <p className="flex items-start gap-2 rounded-md bg-muted/40 px-3 py-2 text-xs text-muted-foreground">
          <AlertTriangle className="mt-0.5 size-3.5 shrink-0" />
          <span>
            Turned off — no agent receives its tools. Its configuration and any
            stored credential survive, and turning it back on restores its tools
            on the next turn.
          </span>
        </p>
      )}

      <section className="space-y-3">
        <h3 className="text-sm font-medium">Tools &amp; permissions</h3>
        <McpToolPermissions
          client={client}
          company={company}
          server={server}
          canManage={canManage}
          agents={agents}
          approvalsPark={approvalsPark}
          reloadKey={reloadKey}
          focus={focusPermissions}
        />
      </section>

      {bridge !== "absent" && server.enabled && reach !== undefined && reach.length === 0 && (
        <p
          className="flex items-start gap-2 rounded-md border border-destructive/30 bg-destructive/10 px-3 py-2 text-xs font-medium text-destructive"
          data-testid="mcp-page-reachability"
        >
          <AlertTriangle className="mt-0.5 size-3.5 shrink-0" />
          <span>
            No agent can reach this server yet — no teammate&apos;s tool grants
            cover <code className="font-mono">{server.accessGrant ?? `mcp:${server.name}`}</code>.
          </span>
        </p>
      )}

      {bridge !== "absent" && server.agentAccess !== undefined ? (
        <McpAgentAccess
          client={client}
          company={company}
          server={server}
          canManage={canManage}
          onSaved={onAccessSaved}
        />
      ) : (
        bridge !== "absent" &&
        standing.live &&
        reach !== undefined &&
        reach.length > 0 && (
          <section className="space-y-2">
            <h3 className="text-sm font-medium">Agents with access</h3>
            <div className="flex flex-wrap gap-1.5" data-testid="mcp-page-reach-list">
              {reach.map((agent) => (
                <Badge key={agent.id} variant="secondary" className="font-normal">
                  {agent.name}
                </Badge>
              ))}
            </div>
          </section>
        )
      )}

      <UsageSection
        usage={usage}
        perConnection={`Successful tool calls your agents made through ${server.name}.`}
      />

      <Dialog open={detailsOpen} onOpenChange={setDetailsOpen}>
        <DialogContent className="sm:max-w-lg" data-testid="mcp-page-details">
          <DialogHeader>
            <DialogTitle>Connection details</DialogTitle>
          </DialogHeader>
          <dl className="space-y-3 text-xs">
            <div className="space-y-1">
              <dt className="text-muted-foreground">Name</dt>
              <dd>
                <code className="font-mono select-text" data-testid="mcp-page-name">
                  {server.name}
                </code>
              </dd>
            </div>
            <div className="space-y-1">
              <dt className="text-muted-foreground">Endpoint</dt>
              <dd>
                <code className="block rounded-md border border-border bg-muted/40 px-2 py-1 font-mono break-all select-text">
                  {server.endpoint}
                </code>
              </dd>
            </div>
            <div className="space-y-1">
              <dt className="text-muted-foreground">Last check</dt>
              <dd data-testid="mcp-page-probe">
                {standing.probe}
                {probedOn(health?.checkedAtMillis) !== null &&
                  ` · ${probedOn(health?.checkedAtMillis)}`}
              </dd>
            </div>
            <div className="space-y-1">
              <dt className="text-muted-foreground">Connected</dt>
              <dd data-testid="mcp-page-connected-on">
                {connectedOn(undefined)} —{" "}
                {server.source === "registry"
                  ? "this host connects a directory install but records no date for it."
                  : "MCP has no connect step to record one."}
              </dd>
            </div>
            {server.websiteUrl && (
              <div className="space-y-1">
                <dt className="text-muted-foreground">Website</dt>
                <dd>
                  <a
                    href={server.websiteUrl}
                    target="_blank"
                    rel="noopener noreferrer"
                    className="break-all underline"
                    data-testid="mcp-page-website"
                  >
                    {server.websiteUrl}
                  </a>
                </dd>
              </div>
            )}
            <div className="space-y-1">
              <dt className="text-muted-foreground">Removing it</dt>
              <dd data-testid="mcp-page-disconnect-scope">
                {mcpRemovalNote(server.source)} Nothing is revoked at the
                server&apos;s own end: no token it issued is invalidated and no
                session there is closed.
              </dd>
              <dd className="text-muted-foreground">{mcpProvenanceNote(server.source)}</dd>
            </div>
          </dl>
        </DialogContent>
      </Dialog>
    </div>
  );
}

/**
 * What this server is for, and the field that sets it.
 *
 * What the server calls itself is offered as the starting point, never written
 * over a declaration.
 */
function McpDescription({
  client,
  company,
  server,
  canManage,
}: {
  client: OpenCompanyClient;
  company: string | null;
  server: McpServer;
  canManage: boolean;
}) {
  const declared = server.description?.trim() ?? "";
  const probed = server.probedDescription?.trim() ?? "";
  // Only a console-added server: a manifest declaration is re-read from
  // `company.toml` on every boot, so a description saved over one would not
  // survive a restart.
  const editable = canManage && server.source === "runtime";
  const [editing, setEditing] = useState(false);
  const [draft, setDraft] = useState(declared || probed);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);
  // What the last successful save wrote, so the view reflects it even before
  // a reload replaces the (now stale) `server` prop.
  const [saved, setSaved] = useState<string | null>(null);

  async function save() {
    setSaving(true);
    setError(null);
    try {
      const next = draft.trim();
      await updateMcpServer(client, company, server.name, {
        description: next,
      });
      setSaved(next);
      setEditing(false);
    } catch (err) {
      setError(
        err instanceof ApiError
          ? err.message
          : "Couldn't save that description.",
      );
    } finally {
      setSaving(false);
    }
  }

  if (editing) {
    return (
      <div className="max-w-prose space-y-1" data-testid="mcp-page-description-edit">
        <Textarea
          rows={2}
          value={draft}
          aria-label={`What ${server.name} does`}
          onChange={(e) => setDraft(e.target.value)}
        />
        {probed && draft.trim() !== probed && (
          <button
            type="button"
            className="text-3xs text-muted-foreground underline"
            data-testid="mcp-page-use-probed"
            onClick={() => setDraft(probed)}
          >
            Use what this server calls itself: {probed}
          </button>
        )}
        {error && <p className="text-xs text-destructive">{error}</p>}
        <div className="flex items-center gap-2">
          <Button size="sm" disabled={saving} onClick={() => void save()}>
            {saving ? "Saving…" : "Save"}
          </Button>
          <Button
            size="sm"
            variant="ghost"
            disabled={saving}
            onClick={() => setEditing(false)}
          >
            Cancel
          </Button>
        </div>
      </div>
    );
  }

  const shown = (saved ?? declared) || probed;
  return (
    <p className="text-sm text-muted-foreground" data-testid="mcp-page-description">
      {shown || "No description — this server says nothing about itself, and none was declared."}
      {editable && (
        <button
          type="button"
          className="ml-2 underline"
          data-testid="mcp-page-describe"
          onClick={() => {
            setDraft(shown);
            setEditing(true);
          }}
        >
          {shown ? "Edit" : "Describe it"}
        </button>
      )}
    </p>
  );
}
