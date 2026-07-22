import { useCallback, useEffect, useState } from "react";
import { Check, ChevronDown, Info, Loader2, Plug, Plus, Server, Trash2 } from "lucide-react";
import { toast } from "sonner";

import type { OpenCompanyClient } from "@/api/client";
import {
  addMcpServer,
  discoverMcpTools,
  listMcpServers,
  removeMcpServer,
  updateMcpServer,
} from "@/api/mcp";
import { ApiError, type ConnectionState, type McpServer, type McpToolInfo } from "@/api/types";
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Skeleton } from "@/components/ui/skeleton";
import { Switch } from "@/components/ui/switch";
import {
  CONNECTION_CATEGORY_ORDER,
  CONNECTION_PROVIDERS,
  type ConnectionProvider,
} from "@/lib/connections";
import { cn } from "@/lib/utils";

interface Props {
  client: OpenCompanyClient;
  company: string | null;
}

type Load = "loading" | "ready" | "unavailable";

/** Wire the third-party accounts your company can act through. */
export function ConnectionsView({ client, company }: Props) {
  const [load, setLoad] = useState<Load>("loading");
  const [states, setStates] = useState<Record<string, ConnectionState>>({});
  const [busy, setBusy] = useState<string | null>(null);

  const refresh = useCallback(async () => {
    try {
      const list = await client.listConnections(company);
      setStates(Object.fromEntries(list.map((c) => [c.provider, c])));
      setLoad("ready");
    } catch {
      // No connections surface on this host yet — show the catalog read-only.
      setLoad("unavailable");
    }
  }, [client, company]);

  useEffect(() => {
    setLoad("loading");
    void refresh();
  }, [refresh]);

  async function connect(p: ConnectionProvider) {
    if (busy) return;
    setBusy(p.id);
    try {
      const { url } = await client.startConnection(p.id, company);
      window.location.href = url;
    } catch {
      toast.error(`Couldn't start the ${p.name} connection.`);
      setBusy(null);
    }
  }

  async function disconnect(p: ConnectionProvider) {
    if (busy) return;
    setBusy(p.id);
    try {
      await client.disconnectConnection(p.id, company);
      toast.success(`Disconnected ${p.name}.`);
      await refresh();
    } catch {
      toast.error(`Couldn't disconnect ${p.name}.`);
    } finally {
      setBusy(null);
    }
  }

  const connectedCount = Object.values(states).filter((s) => s.connected).length;

  return (
    <div className="flex-1 overflow-y-auto">
      <div className="mx-auto w-full max-w-5xl space-y-6 px-4 py-6">
        <div className="flex flex-wrap items-center justify-between gap-3">
          <div className="space-y-1">
            <h2 className="text-2xl font-semibold tracking-tight">Connections</h2>
            <p className="text-sm text-muted-foreground">
              Wire in the accounts your company acts through. It only uses what you connect.
            </p>
          </div>
          {load === "ready" && connectedCount > 0 && (
            <Badge variant="secondary">{connectedCount} connected</Badge>
          )}
        </div>

        {load === "unavailable" && (
          <Alert>
            <Info className="size-4" />
            <AlertTitle>Connections aren&apos;t wired on this host yet</AlertTitle>
            <AlertDescription>
              The catalog below shows what your company can connect once the host exposes its OAuth
              endpoints. Connecting is disabled until then.
            </AlertDescription>
          </Alert>
        )}

        <McpServersSection client={client} company={company} />

        {load === "loading" ? (
          <div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-3">
            {Array.from({ length: 6 }).map((_, i) => (
              <Skeleton key={i} className="h-28 rounded-xl" />
            ))}
          </div>
        ) : (
          CONNECTION_CATEGORY_ORDER.map((category) => {
            const providers = CONNECTION_PROVIDERS.filter((p) => p.category === category);
            if (providers.length === 0) return null;
            return (
              <section key={category} className="space-y-3">
                <h3 className="text-xs font-medium tracking-wide text-muted-foreground uppercase">
                  {category}
                </h3>
                <div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-3">
                  {providers.map((p) => (
                    <ConnectionCard
                      key={p.id}
                      provider={p}
                      state={states[p.id]}
                      disabled={load === "unavailable"}
                      busy={busy === p.id}
                      onConnect={() => void connect(p)}
                      onDisconnect={() => void disconnect(p)}
                    />
                  ))}
                </div>
              </section>
            );
          })
        )}
      </div>
    </div>
  );
}

function ConnectionCard({
  provider,
  state,
  disabled,
  busy,
  onConnect,
  onDisconnect,
}: {
  provider: ConnectionProvider;
  state?: ConnectionState;
  disabled: boolean;
  busy: boolean;
  onConnect: () => void;
  onDisconnect: () => void;
}) {
  const connected = Boolean(state?.connected);
  return (
    <Card className={cn(connected && "border-primary/30")}>
      <CardContent className="flex h-full flex-col gap-3 py-4">
        <div className="flex items-start gap-3">
          <Monogram provider={provider} />
          <div className="min-w-0 flex-1">
            <div className="flex items-center gap-2">
              <p className="truncate font-medium">{provider.name}</p>
              {connected && (
                <span className="inline-flex items-center gap-1 text-xs font-medium text-emerald-600 dark:text-emerald-400">
                  <Check className="size-3" /> Connected
                </span>
              )}
            </div>
            <p className="mt-0.5 line-clamp-2 text-xs text-muted-foreground">
              {connected && state?.account ? state.account : provider.description}
            </p>
          </div>
        </div>
        <div className="mt-auto">
          {connected ? (
            <Button variant="outline" size="sm" className="w-full" disabled={busy} onClick={onDisconnect}>
              {busy ? <Loader2 className="size-4 animate-spin" /> : null}
              Disconnect
            </Button>
          ) : (
            <Button
              variant={disabled ? "outline" : "default"}
              size="sm"
              className="w-full"
              disabled={disabled || busy}
              onClick={onConnect}
            >
              {busy ? <Loader2 className="size-4 animate-spin" /> : <Plug className="size-4" />}
              Connect
            </Button>
          )}
        </div>
      </CardContent>
    </Card>
  );
}

function Monogram({ provider }: { provider: ConnectionProvider }) {
  const label = provider.glyph ?? provider.name.charAt(0);
  return (
    <div
      className="flex size-10 shrink-0 items-center justify-center rounded-lg text-sm font-semibold text-white"
      style={{ backgroundColor: provider.color }}
      aria-hidden
    >
      {label}
    </div>
  );
}

type McpLoad = "loading" | "ready" | "unavailable";
type ToolsState =
  | { kind: "idle" }
  | { kind: "loading" }
  | { kind: "unwired" }
  | { kind: "error"; message: string }
  | { kind: "ready"; tools: McpToolInfo[] };

/**
 * Manage the company's MCP tool servers (issue #50). Lists the effective set
 * (manifest + runtime), adds runtime servers with a **write-only** token field,
 * toggles/removes them, and live-discovers each server's tools. A manifest
 * server can be disabled but not deleted.
 */
function McpServersSection({
  client,
  company,
}: {
  client: OpenCompanyClient;
  company: string | null;
}) {
  const [load, setLoad] = useState<McpLoad>("loading");
  const [servers, setServers] = useState<McpServer[]>([]);
  const [busy, setBusy] = useState<string | null>(null);
  const [tools, setTools] = useState<Record<string, ToolsState>>({});

  // Add-server form.
  const [name, setName] = useState("");
  const [endpoint, setEndpoint] = useState("");
  const [token, setToken] = useState("");

  const refresh = useCallback(async () => {
    try {
      setServers(await listMcpServers(client, company));
      setLoad("ready");
    } catch {
      setLoad("unavailable");
    }
  }, [client, company]);

  useEffect(() => {
    setLoad("loading");
    void refresh();
  }, [refresh]);

  async function add() {
    if (busy) return;
    if (!name.trim() || !endpoint.trim()) {
      toast.error("A server needs a name and an https endpoint.");
      return;
    }
    setBusy("add");
    try {
      await addMcpServer(client, company, {
        name: name.trim(),
        endpoint: endpoint.trim(),
        token: token.trim() || undefined,
      });
      toast.success(`Added ${name.trim()}. Agents pick it up on the next rebuild.`);
      setName("");
      setEndpoint("");
      setToken("");
      await refresh();
    } catch (err) {
      toast.error(err instanceof ApiError ? err.message : "Couldn't add the server.");
    } finally {
      setBusy(null);
    }
  }

  async function toggle(server: McpServer, enabled: boolean) {
    if (busy) return;
    setBusy(server.name);
    try {
      await updateMcpServer(client, company, server.name, { enabled });
      await refresh();
    } catch (err) {
      toast.error(err instanceof ApiError ? err.message : "Couldn't update the server.");
    } finally {
      setBusy(null);
    }
  }

  async function remove(server: McpServer) {
    if (busy) return;
    setBusy(server.name);
    try {
      await removeMcpServer(client, company, server.name);
      toast.success(`Removed ${server.name}.`);
      await refresh();
    } catch (err) {
      toast.error(err instanceof ApiError ? err.message : "Couldn't remove the server.");
    } finally {
      setBusy(null);
    }
  }

  async function discover(server: McpServer) {
    // Toggle closed if already shown.
    if (tools[server.name]?.kind === "ready") {
      setTools((t) => ({ ...t, [server.name]: { kind: "idle" } }));
      return;
    }
    setTools((t) => ({ ...t, [server.name]: { kind: "loading" } }));
    try {
      const list = await discoverMcpTools(client, company, server.name);
      setTools((t) => ({ ...t, [server.name]: { kind: "ready", tools: list } }));
    } catch (err) {
      if (err instanceof ApiError && err.code === "not_wired") {
        setTools((t) => ({ ...t, [server.name]: { kind: "unwired" } }));
      } else {
        setTools((t) => ({
          ...t,
          [server.name]: {
            kind: "error",
            message: err instanceof ApiError ? err.message : "Discovery failed.",
          },
        }));
      }
    }
  }

  if (load === "unavailable") return null;

  return (
    <section className="space-y-3">
      <div className="flex items-center gap-2">
        <Server className="size-4 text-muted-foreground" />
        <h3 className="text-xs font-medium tracking-wide text-muted-foreground uppercase">
          MCP Servers
        </h3>
      </div>
      <p className="text-sm text-muted-foreground">
        Remote MCP tool servers your agents can call. Add an HTTP endpoint and (optionally) a token —
        the token is stored securely and never shown again.
      </p>

      {load === "loading" ? (
        <Skeleton className="h-24 rounded-xl" />
      ) : (
        <Card>
          <CardContent className="space-y-3 py-4">
            {servers.length === 0 ? (
              <p className="text-sm text-muted-foreground">No MCP servers yet.</p>
            ) : (
              <ul className="divide-y divide-border">
                {servers.map((server) => (
                  <li key={server.name} className="space-y-2 py-3 first:pt-0 last:pb-0">
                    <div className="flex flex-wrap items-center gap-2">
                      <span className="font-medium">{server.name}</span>
                      <Badge variant={server.source === "manifest" ? "secondary" : "outline"}>
                        {server.source}
                      </Badge>
                      {server.authConfigured && (
                        <span className="inline-flex items-center gap-1 text-xs text-emerald-600 dark:text-emerald-400">
                          <Check className="size-3" /> auth set
                        </span>
                      )}
                      <span className="ml-auto flex items-center gap-2">
                        <Switch
                          checked={server.enabled}
                          disabled={busy === server.name}
                          onCheckedChange={(v) => void toggle(server, v)}
                          aria-label={`Enable ${server.name}`}
                        />
                        <Button
                          variant="ghost"
                          size="sm"
                          disabled={busy === server.name}
                          onClick={() => void discover(server)}
                        >
                          <ChevronDown className="size-4" /> Tools
                        </Button>
                        {server.source === "runtime" && (
                          <Button
                            variant="ghost"
                            size="sm"
                            disabled={busy === server.name}
                            onClick={() => void remove(server)}
                            aria-label={`Remove ${server.name}`}
                          >
                            <Trash2 className="size-4" />
                          </Button>
                        )}
                      </span>
                    </div>
                    <p className="truncate text-xs text-muted-foreground">{server.endpoint}</p>
                    <McpToolsList state={tools[server.name] ?? { kind: "idle" }} />
                  </li>
                ))}
              </ul>
            )}

            <div className="grid gap-2 border-t border-border pt-3 sm:grid-cols-[1fr_1.5fr_1fr_auto] sm:items-end">
              <div className="space-y-1">
                <Label htmlFor="mcp-name" className="text-xs">
                  Name
                </Label>
                <Input
                  id="mcp-name"
                  value={name}
                  placeholder="notion"
                  onChange={(e) => setName(e.target.value)}
                />
              </div>
              <div className="space-y-1">
                <Label htmlFor="mcp-endpoint" className="text-xs">
                  Endpoint
                </Label>
                <Input
                  id="mcp-endpoint"
                  value={endpoint}
                  placeholder="https://host/mcp"
                  onChange={(e) => setEndpoint(e.target.value)}
                />
              </div>
              <div className="space-y-1">
                <Label htmlFor="mcp-token" className="text-xs">
                  Token (optional)
                </Label>
                <Input
                  id="mcp-token"
                  type="password"
                  value={token}
                  placeholder="write-only"
                  autoComplete="off"
                  onChange={(e) => setToken(e.target.value)}
                />
              </div>
              <Button disabled={busy === "add"} onClick={() => void add()}>
                {busy === "add" ? <Loader2 className="size-4 animate-spin" /> : <Plus className="size-4" />}
                Add
              </Button>
            </div>
          </CardContent>
        </Card>
      )}
    </section>
  );
}

/** Renders the live-discovered tool list for one server. */
function McpToolsList({ state }: { state: ToolsState }) {
  if (state.kind === "idle") return null;
  if (state.kind === "loading") {
    return (
      <p className="flex items-center gap-1 text-xs text-muted-foreground">
        <Loader2 className="size-3 animate-spin" /> Discovering tools…
      </p>
    );
  }
  if (state.kind === "unwired") {
    return (
      <p className="text-xs text-muted-foreground">
        Live tool discovery isn&apos;t enabled in this build (the agent harness is off).
      </p>
    );
  }
  if (state.kind === "error") {
    return <p className="text-xs text-destructive">{state.message}</p>;
  }
  if (state.tools.length === 0) {
    return <p className="text-xs text-muted-foreground">This server exposed no tools.</p>;
  }
  return (
    <ul className="space-y-1 rounded-md bg-muted/40 p-2">
      {state.tools.map((tool) => (
        <li key={tool.name} className="text-xs">
          <span className="font-mono font-medium">{tool.name}</span>
          {tool.description ? (
            <span className="text-muted-foreground"> — {tool.description}</span>
          ) : null}
        </li>
      ))}
    </ul>
  );
}
