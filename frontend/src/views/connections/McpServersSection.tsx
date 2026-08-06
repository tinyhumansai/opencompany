import { useCallback, useEffect, useRef, useState } from "react";
import {
  AlertTriangle,
  Check,
  ChevronDown,
  Info,
  Loader2,
  LogIn,
  Plug,
  Plus,
  Server,
  Trash2,
} from "lucide-react";
import { toast } from "sonner";

import type { OpenCompanyClient } from "@/api/client";
import {
  addMcpServer,
  discoverMcpTools,
  listMcpServers,
  type McpAuthKind,
  removeMcpServer,
  startMcpOAuth,
  testMcpServer,
  updateMcpServer,
} from "@/api/mcp";
import { ApiError, type McpHealth, type McpServer, type McpToolInfo } from "@/api/types";
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Skeleton } from "@/components/ui/skeleton";
import { Switch } from "@/components/ui/switch";

type McpLoad = "loading" | "ready" | "unavailable" | "error";
type ToolsState =
  | { kind: "idle" }
  | { kind: "loading" }
  | { kind: "unwired" }
  | { kind: "error"; message: string }
  | { kind: "ready"; tools: McpToolInfo[] };

/**
 * How the page around this section frames it.
 *
 * - `inline` — a section among others (Connections). The page has plenty else
 *   to show, so a host with no MCP surface renders nothing here at all.
 * - `standalone` — the whole page is this section (Settings, MCP Servers). The
 *   page supplies the heading, and a host with no MCP surface says so, because
 *   the alternative is a page that is simply blank.
 */
export type McpSectionChrome = "inline" | "standalone";

interface Props {
  client: OpenCompanyClient;
  company: string | null;
  /** Whether this viewer may add, edit or remove servers (issue #403). */
  canManage: boolean;
  chrome?: McpSectionChrome;
}

/**
 * Manage the company's MCP tool servers (issue #50). Lists the effective set
 * (manifest + runtime), adds runtime servers with a **write-only** token field,
 * toggles/removes them, and live-discovers each server's tools. A manifest
 * server can be disabled but not deleted.
 *
 * This is the console's **only** MCP surface, reached from two places: the
 * Connections page renders it `inline`, Settings, MCP Servers renders it
 * `standalone`. Settings used to carry a second implementation of this screen
 * against an API no host has ever served (`{ servers }` wrappers, `server_id`
 * keys, `/connect` and `/disconnect` routes), which crashed on open — a second
 * surface is how the two came to disagree, so there is one (issue #414).
 */
export function McpServersSection({ client, company, canManage, chrome = "inline" }: Props) {
  const [load, setLoad] = useState<McpLoad>("loading");
  const [servers, setServers] = useState<McpServer[]>([]);
  const [busy, setBusy] = useState<string | null>(null);
  const [tools, setTools] = useState<Record<string, ToolsState>>({});
  // Live health from an on-demand Test, overriding the persisted badge per row.
  const [tested, setTested] = useState<Record<string, McpHealth>>({});
  // In-flight OAuth sign-in poll timers, keyed by server name. A row with a live
  // timer is still "signing in" even after its `busy` flag clears, so a repeat
  // click can't spawn a second overlapping poll. Cleared on unmount so stale
  // callbacks don't fire against a gone component.
  const pollTimers = useRef<Record<string, number>>({});

  // Add-server form.
  const [name, setName] = useState("");
  const [endpoint, setEndpoint] = useState("");
  const [token, setToken] = useState("");
  const [authKind, setAuthKind] = useState<McpAuthKind>("bearer");
  const [authFieldName, setAuthFieldName] = useState("");
  // The add flow's failure is a PERSISTENT inline alert (not a transient toast):
  // a silent-fail auth error is exactly the bug this cell fixes.
  const [addError, setAddError] = useState<string | null>(null);

  const refresh = useCallback(async () => {
    try {
      setServers(await listMcpServers(client, company));
      setLoad("ready");
    } catch (err) {
      // A 404 is a host with no MCP surface: a fact about the build, not a
      // failure. Anything else (offline, 5xx, a body that wasn't the list the
      // route promises) means we do not know what this company has, and saying
      // "no MCP here" would be a claim we cannot make (issue #414).
      setLoad(err instanceof ApiError && err.status === 404 ? "unavailable" : "error");
    }
  }, [client, company]);

  useEffect(() => {
    setLoad("loading");
    void refresh();
  }, [refresh]);

  // Cancel any in-flight sign-in polls when the view unmounts so their timers
  // don't fire against a torn-down component.
  useEffect(() => {
    const timers = pollTimers.current;
    return () => {
      for (const id of Object.values(timers)) window.clearTimeout(id);
    };
  }, []);

  async function add() {
    if (busy) return;
    setAddError(null);
    if (!name.trim() || !endpoint.trim()) {
      setAddError("A server needs a name and an https endpoint.");
      return;
    }
    if (authKind !== "bearer" && token.trim() && !authFieldName.trim()) {
      setAddError(
        authKind === "header"
          ? "A custom-header credential needs a header name."
          : "A query-parameter credential needs a parameter name.",
      );
      return;
    }
    setBusy("add");
    try {
      const res = await addMcpServer(client, company, {
        name: name.trim(),
        endpoint: endpoint.trim(),
        token: token.trim() || undefined,
        authKind,
        headerName: authKind === "header" ? authFieldName.trim() || undefined : undefined,
        paramName: authKind === "query_param" ? authFieldName.trim() || undefined : undefined,
      });
      // A probe that lands "needs config" or "error" is NOT a rollback — the
      // server is added — but surface it inline so the operator acts on it.
      // Exception: an OAuth-required result is not an error to shout about — the
      // amber "needs config" badge carries a Sign in button, so a red alert here
      // would be redundant and misleading.
      if (res.test && res.test.status !== "ok" && res.test.authHint !== "oauth_required") {
        setAddError(res.test.message);
      } else if (res.warning) {
        setAddError(res.warning);
      } else {
        toast.success(`Added ${name.trim()}. Agents pick it up on the next rebuild.`);
      }
      setName("");
      setEndpoint("");
      setToken("");
      setAuthFieldName("");
      await refresh();
    } catch (err) {
      // Persistent, not a toast: the operator must see why the add failed.
      setAddError(err instanceof ApiError ? err.message : "Couldn't add the server.");
    } finally {
      setBusy(null);
    }
  }

  async function test(server: McpServer) {
    if (busy) return;
    setBusy(server.name);
    try {
      const health = await testMcpServer(client, company, server.name);
      setTested((t) => ({ ...t, [server.name]: health }));
    } catch (err) {
      if (err instanceof ApiError && err.code === "not_wired") {
        toast.message("Live testing isn't enabled in this build (the agent harness is off).");
      } else {
        toast.error(err instanceof ApiError ? err.message : "Couldn't test the server.");
      }
    } finally {
      setBusy(null);
    }
  }

  // Browser OAuth sign-in (issue #90): open the authorization URL in a new tab,
  // then poll the server's health until it flips to `ok` (the host stores the
  // token on its callback route) so the amber badge turns green on its own.
  async function signIn(server: McpServer) {
    // Guard both the shared `busy` flag and a per-server poll already in flight:
    // the poll outlives `busy`, so without the second check a repeat click would
    // spawn a second overlapping sign-in (duplicate token exchange + toasts).
    if (busy || pollTimers.current[server.name] !== undefined) return;
    setBusy(server.name);
    try {
      const { authorizeUrl } = await startMcpOAuth(client, company, server.name);
      window.open(authorizeUrl, "_blank", "noopener,noreferrer");
      toast.message(`Complete sign-in for ${server.name} in the new tab.`);
      // Poll for completion for up to ~2 minutes; stop as soon as it's healthy.
      const deadline = Date.now() + 120_000;
      const poll = async () => {
        delete pollTimers.current[server.name];
        if (Date.now() > deadline) {
          toast.message(
            `Sign-in for ${server.name} timed out. Try again if it didn't complete.`,
          );
          return;
        }
        try {
          const health = await testMcpServer(client, company, server.name);
          setTested((t) => ({ ...t, [server.name]: health }));
          if (health.status === "ok") {
            toast.success(`Signed in to ${server.name}.`);
            await refresh();
            return;
          }
        } catch {
          // Ignore transient probe errors while the operator finishes sign-in.
        }
        pollTimers.current[server.name] = window.setTimeout(() => void poll(), 2_000);
      };
      pollTimers.current[server.name] = window.setTimeout(() => void poll(), 2_000);
    } catch (err) {
      if (err instanceof ApiError && err.code === "not_wired") {
        toast.message("OAuth sign-in isn't enabled in this build (the agent harness is off).");
      } else {
        toast.error(err instanceof ApiError ? err.message : "Couldn't start sign-in.");
      }
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

  if (load === "unavailable") {
    if (chrome === "inline") return null;
    return (
      <Alert data-testid="mcp-unavailable">
        <Info className="size-4" />
        <AlertTitle>MCP servers aren&apos;t wired on this host</AlertTitle>
        <AlertDescription>
          This host serves no MCP routes, so there is nothing to manage here yet.
        </AlertDescription>
      </Alert>
    );
  }

  return (
    <section className="space-y-3">
      {chrome === "inline" && (
        <div className="flex items-center gap-2">
          <Server className="size-4 text-muted-foreground" />
          <h3 className="text-xs font-medium tracking-wide text-muted-foreground uppercase">
            MCP Servers
          </h3>
        </div>
      )}
      <p className="text-sm text-muted-foreground">
        Remote MCP tool servers your agents can call. Add an HTTP endpoint and (optionally) a token —
        the token is stored securely and never shown again.
      </p>

      {load === "error" ? (
        // Not an empty list: an empty list is a company with no tool servers,
        // and this host did not tell us that (issue #414).
        <Alert variant="destructive" data-testid="mcp-load-error">
          <AlertTriangle className="size-4" />
          <AlertTitle>Couldn&apos;t load this company&apos;s MCP servers</AlertTitle>
          <AlertDescription>
            The host didn&apos;t answer with its server list, so what is installed is unknown.
            Reload to try again.
          </AlertDescription>
        </Alert>
      ) : load === "loading" ? (
        <Skeleton className="h-24 rounded-xl" />
      ) : (
        <Card>
          <CardContent className="space-y-3 py-4">
            {servers.length === 0 ? (
              <p className="text-sm text-muted-foreground">No MCP servers yet.</p>
            ) : (
              <ul className="divide-y divide-border">
                {servers.map((server) => {
                  const health = tested[server.name] ?? server.health;
                  return (
                    <li
                      key={server.name}
                      data-testid="mcp-server-row"
                      className="space-y-2 py-3 first:pt-0 last:pb-0"
                    >
                      <div className="flex flex-wrap items-center gap-2">
                        <span className="font-medium">{server.name}</span>
                        <Badge variant={server.source === "manifest" ? "secondary" : "outline"}>
                          {server.source}
                        </Badge>
                        <McpHealthBadge health={health} authConfigured={server.authConfigured} />
                        <span className="ml-auto flex items-center gap-2">
                          <Switch
                            checked={server.enabled}
                            disabled={busy === server.name || !canManage}
                            onCheckedChange={(v) => void toggle(server, v)}
                            aria-label={`Enable ${server.name}`}
                          />
                          {health?.authHint === "oauth_required" && canManage && (
                            <Button
                              variant="default"
                              size="sm"
                              disabled={busy === server.name}
                              onClick={() => void signIn(server)}
                            >
                              {busy === server.name ? (
                                <Loader2 className="size-4 animate-spin" />
                              ) : (
                                <LogIn className="size-4" />
                              )}{" "}
                              Sign in
                            </Button>
                          )}
                          <Button
                            variant="ghost"
                            size="sm"
                            disabled={busy === server.name}
                            onClick={() => void test(server)}
                          >
                            {busy === server.name ? (
                              <Loader2 className="size-4 animate-spin" />
                            ) : (
                              <Plug className="size-4" />
                            )}{" "}
                            Test
                          </Button>
                          <Button
                            variant="ghost"
                            size="sm"
                            disabled={busy === server.name}
                            onClick={() => void discover(server)}
                          >
                            <ChevronDown className="size-4" /> Tools
                          </Button>
                          {server.source === "runtime" && canManage && (
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
                      {health && health.status !== "ok" && health.message && (
                        <p className="text-xs text-muted-foreground">{health.message}</p>
                      )}
                      <McpToolsList state={tools[server.name] ?? { kind: "idle" }} />
                    </li>
                  );
                })}
              </ul>
            )}

            {/* Adding a server hands the agents a new set of tools, so the
                whole form goes for a member rather than leaving fields that
                cannot be submitted (issue #403). Test and Tools above stay:
                they probe a server an admin already added, and the host
                deliberately leaves those open. */}
            {canManage && (
              <div className="space-y-2 border-t border-border pt-3">
                {addError && (
                  <Alert variant="destructive">
                    <AlertTriangle className="size-4" />
                    <AlertTitle>Couldn&apos;t add the server</AlertTitle>
                    <AlertDescription>{addError}</AlertDescription>
                  </Alert>
                )}
                <div className="grid gap-2 sm:grid-cols-2 sm:items-end">
                  <div className="space-y-1">
                    <Label htmlFor="mcp-name" className="text-xs">
                      Name
                    </Label>
                    <Input
                      id="mcp-name"
                      data-testid="mcp-add-name"
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
                      name="mcp-endpoint-url"
                      data-testid="mcp-add-endpoint"
                      value={endpoint}
                      placeholder="https://host/mcp"
                      autoComplete="url"
                      onChange={(e) => setEndpoint(e.target.value)}
                    />
                  </div>
                </div>
                <div className="grid gap-2 sm:grid-cols-[auto_1fr_1fr_auto] sm:items-end">
                  <div className="space-y-1">
                    <Label htmlFor="mcp-auth-kind" className="text-xs">
                      Auth
                    </Label>
                    <select
                      id="mcp-auth-kind"
                      value={authKind}
                      onChange={(e) => setAuthKind(e.target.value as McpAuthKind)}
                      className="h-9 w-full rounded-md border border-input bg-transparent px-3 text-sm shadow-xs"
                    >
                      <option value="bearer">Bearer token</option>
                      <option value="header">Custom header</option>
                      <option value="query_param">Query parameter</option>
                    </select>
                  </div>
                  {authKind !== "bearer" && (
                    <div className="space-y-1">
                      <Label htmlFor="mcp-auth-field" className="text-xs">
                        {authKind === "header" ? "Header name" : "Parameter name"}
                      </Label>
                      <Input
                        id="mcp-auth-field"
                        value={authFieldName}
                        placeholder={authKind === "header" ? "X-Api-Key" : "apiKey"}
                        autoComplete="off"
                        onChange={(e) => setAuthFieldName(e.target.value)}
                      />
                    </div>
                  )}
                  <div className="space-y-1">
                    <Label htmlFor="mcp-token" className="text-xs">
                      {authKind === "bearer" ? "Token (optional)" : "Credential value"}
                    </Label>
                    <Input
                      id="mcp-token"
                      name="mcp-token-secret"
                      type="password"
                      value={token}
                      placeholder="write-only"
                      autoComplete="new-password"
                      onChange={(e) => setToken(e.target.value)}
                    />
                  </div>
                  <Button
                    data-testid="mcp-add-submit"
                    disabled={busy === "add"}
                    onClick={() => void add()}
                  >
                    {busy === "add" ? (
                      <Loader2 className="size-4 animate-spin" />
                    ) : (
                      <Plus className="size-4" />
                    )}
                    Add
                  </Button>
                </div>
              </div>
            )}
          </CardContent>
        </Card>
      )}
    </section>
  );
}

/**
 * The per-server health badge: green `ok · N tools`, amber `needs config`, red
 * `error`. Falls back to a plain "auth set" hint when the server has never been
 * probed (no `health`).
 */
function McpHealthBadge({
  health,
  authConfigured,
}: {
  health?: McpHealth;
  authConfigured: boolean;
}) {
  if (!health) {
    // Never probed — show only the non-secret auth hint (unchanged behavior).
    return authConfigured ? (
      <span className="inline-flex items-center gap-1 text-xs text-emerald-600 dark:text-emerald-400">
        <Check className="size-3" /> auth set
      </span>
    ) : null;
  }
  if (health.status === "ok") {
    return (
      <span className="inline-flex items-center gap-1 text-xs text-emerald-600 dark:text-emerald-400">
        <Check className="size-3" /> ok · {health.toolCount} tool{health.toolCount === 1 ? "" : "s"}
      </span>
    );
  }
  if (health.status === "needs_config") {
    return (
      <span className="inline-flex items-center gap-1 text-xs text-amber-600 dark:text-amber-400">
        <AlertTriangle className="size-3" /> needs config
      </span>
    );
  }
  if (health.status === "error") {
    return (
      <span className="inline-flex items-center gap-1 text-xs text-destructive">
        <AlertTriangle className="size-3" /> error
      </span>
    );
  }
  return null;
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
