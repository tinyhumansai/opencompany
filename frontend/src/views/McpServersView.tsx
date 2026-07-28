import { type FormEvent, useCallback, useEffect, useState } from "react";
import { ChevronDown, ChevronRight, Info, Loader2, Play, Plug, Trash2, Unplug } from "lucide-react";
import { toast } from "sonner";

import { ApiError } from "@/api/types";
import type { OpenCompanyClient } from "@/api/client";
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Textarea } from "@/components/ui/textarea";
import type { McpServer, McpTool, McpTransport } from "@/lib/mcp";

interface Props {
  client: OpenCompanyClient;
  company: string | null;
}

type Load = "loading" | "ready" | "unavailable";

export function McpServersView({ client, company }: Props) {
  const [load, setLoad] = useState<Load>("loading");
  const [servers, setServers] = useState<McpServer[]>([]);
  const [busy, setBusy] = useState<string | null>(null);
  const [name, setName] = useState("");
  const [transport, setTransport] = useState<McpTransport>("stdio");
  const [command, setCommand] = useState("");
  const [args, setArgs] = useState("");
  const [url, setUrl] = useState("");
  const [env, setEnv] = useState("{}");

  const refresh = useCallback(async () => {
    try {
      const response = await client.listMcpServers(company);
      setServers(response.servers);
      setLoad("ready");
    } catch (error) {
      if (error instanceof ApiError && error.status === 404) setLoad("unavailable");
      else {
        setLoad("ready");
        toast.error("Couldn't load MCP servers.");
      }
    }
  }, [client, company]);

  useEffect(() => {
    setLoad("loading");
    void refresh();
  }, [refresh]);

  async function install(event: FormEvent) {
    event.preventDefault();
    if (busy) return;
    let envValues: Record<string, string>;
    try {
      const parsed: unknown = JSON.parse(env || "{}");
      if (!parsed || Array.isArray(parsed) || typeof parsed !== "object") throw new Error();
      envValues = Object.fromEntries(
        Object.entries(parsed).map(([key, value]) => {
          if (typeof value !== "string") throw new Error();
          return [key, value];
        }),
      );
    } catch {
      toast.error("Environment must be a JSON object with string values.");
      return;
    }

    setBusy("install");
    try {
      await client.installMcpServer(
        {
          name,
          transport,
          command: transport === "stdio" ? command : undefined,
          args: transport === "stdio" ? splitArgs(args) : undefined,
          env: envValues,
          url: transport === "http" ? url : undefined,
        },
        company,
      );
      setName("");
      setCommand("");
      setArgs("");
      setUrl("");
      setEnv("{}");
      toast.success("MCP server installed.");
      await refresh();
    } catch (error) {
      toast.error(error instanceof Error ? error.message : "Couldn't install MCP server.");
    } finally {
      setBusy(null);
    }
  }

  async function lifecycle(server: McpServer, action: "connect" | "disconnect" | "uninstall") {
    if (busy) return;
    setBusy(`${action}:${server.server_id}`);
    try {
      if (action === "connect") await client.connectMcpServer(server.server_id, company);
      else if (action === "disconnect") await client.disconnectMcpServer(server.server_id, company);
      else await client.uninstallMcpServer(server.server_id, company);
      await refresh();
    } catch (error) {
      toast.error(error instanceof Error ? error.message : `Couldn't ${action} MCP server.`);
    } finally {
      setBusy(null);
    }
  }

  return (
    <div className="flex-1 overflow-y-auto">
      <div className="mx-auto w-full max-w-5xl space-y-6 px-4 py-6">
        <div className="space-y-1">
          <h2 className="text-2xl font-semibold tracking-tight">MCP Servers</h2>
          <p className="text-sm text-muted-foreground">
            Install tool servers for operators and company agents to use through the embedded runtime.
          </p>
        </div>

        {load === "unavailable" ? (
          <Alert>
            <Info className="size-4" />
            <AlertTitle>MCP is not enabled on this host</AlertTitle>
            <AlertDescription>
              Rebuild the OpenCompany host with the MCP feature to install and call tool servers.
            </AlertDescription>
          </Alert>
        ) : (
          <>
            <Card>
              <CardHeader>
                <CardTitle>Install a server</CardTitle>
              </CardHeader>
              <CardContent>
                <form className="grid gap-4 md:grid-cols-2" onSubmit={install}>
                  <Field label="Name">
                    <Input
                      data-testid="mcp-install-name"
                      value={name}
                      onChange={(event) => setName(event.target.value)}
                      required
                    />
                  </Field>
                  <Field label="Transport">
                    <select
                      className="h-8 rounded-lg border border-input bg-background px-2.5 text-sm"
                      value={transport}
                      onChange={(event) => setTransport(event.target.value as McpTransport)}
                    >
                      <option value="stdio">Local command (stdio)</option>
                      <option value="http">Remote HTTP</option>
                    </select>
                  </Field>
                  {transport === "stdio" ? (
                    <>
                      <Field label="Command">
                        <Input
                          data-testid="mcp-install-command"
                          value={command}
                          onChange={(event) => setCommand(event.target.value)}
                          placeholder="node"
                          required
                        />
                      </Field>
                      <Field label="Arguments">
                        <Input
                          data-testid="mcp-install-args"
                          value={args}
                          onChange={(event) => setArgs(event.target.value)}
                          placeholder="/path/to/server.mjs"
                        />
                      </Field>
                    </>
                  ) : (
                    <Field label="URL">
                      <Input
                        value={url}
                        onChange={(event) => setUrl(event.target.value)}
                        placeholder="https://example.com/mcp"
                        required
                      />
                    </Field>
                  )}
                  <Field label="Environment JSON" className="md:col-span-2">
                    <Textarea
                      value={env}
                      onChange={(event) => setEnv(event.target.value)}
                      placeholder='{"API_KEY":"…"}'
                      rows={3}
                    />
                    <p className="text-xs text-muted-foreground">
                      Values are write-only and never returned by the API.
                    </p>
                  </Field>
                  <div className="md:col-span-2">
                    <Button data-testid="mcp-install-submit" type="submit" disabled={busy === "install"}>
                      {busy === "install" && <Loader2 className="size-4 animate-spin" />}
                      Install server
                    </Button>
                  </div>
                </form>
              </CardContent>
            </Card>

            <section className="space-y-3">
              <h3 className="font-medium">Installed servers</h3>
              {load === "loading" ? (
                <p className="text-sm text-muted-foreground">Loading MCP servers…</p>
              ) : servers.length === 0 ? (
                <p className="rounded-lg border border-dashed p-6 text-sm text-muted-foreground">
                  No MCP servers installed yet.
                </p>
              ) : (
                servers.map((server) => (
                  <ServerCard
                    key={server.server_id}
                    server={server}
                    client={client}
                    company={company}
                    busy={busy}
                    onLifecycle={(action) => void lifecycle(server, action)}
                  />
                ))
              )}
            </section>
          </>
        )}
      </div>
    </div>
  );
}

function ServerCard({
  server,
  client,
  company,
  busy,
  onLifecycle,
}: {
  server: McpServer;
  client: OpenCompanyClient;
  company: string | null;
  busy: string | null;
  onLifecycle: (action: "connect" | "disconnect" | "uninstall") => void;
}) {
  const [expanded, setExpanded] = useState(false);
  const [tools, setTools] = useState<McpTool[]>([]);
  const [toolName, setToolName] = useState("");
  const [argumentsText, setArgumentsText] = useState("{}");
  const [result, setResult] = useState("");
  const [calling, setCalling] = useState(false);

  async function toggleTools() {
    const next = !expanded;
    setExpanded(next);
    if (!next || tools.length > 0 || server.status !== "connected") return;
    try {
      const response = await client.listMcpTools(server.server_id, company);
      setTools(response.tools);
      setToolName(response.tools[0]?.name ?? "");
    } catch (error) {
      toast.error(error instanceof Error ? error.message : "Couldn't load MCP tools.");
    }
  }

  async function callTool() {
    let arguments_: Record<string, unknown>;
    try {
      const parsed: unknown = JSON.parse(argumentsText);
      if (!parsed || Array.isArray(parsed) || typeof parsed !== "object") throw new Error();
      arguments_ = parsed as Record<string, unknown>;
    } catch {
      toast.error("Tool arguments must be a JSON object.");
      return;
    }
    setCalling(true);
    try {
      const response = await client.callMcpTool(server.server_id, toolName, arguments_, company);
      setResult(JSON.stringify(response.result, null, 2));
    } catch (error) {
      setResult(error instanceof Error ? error.message : "Tool call failed.");
    } finally {
      setCalling(false);
    }
  }

  return (
    <Card data-testid="mcp-server-row">
      <CardContent className="space-y-4 py-4">
        <div className="flex flex-wrap items-center gap-3">
          <Button variant="ghost" size="icon-sm" onClick={() => void toggleTools()} aria-label={`Show ${server.name} tools`}>
            {expanded ? <ChevronDown /> : <ChevronRight />}
          </Button>
          <div className="min-w-0 flex-1">
            <p className="font-medium">{server.name}</p>
            <p className="truncate text-xs text-muted-foreground">
              {server.transport === "stdio" ? [server.command, ...server.args].join(" ") : server.url}
            </p>
          </div>
          <Badge variant={server.status === "connected" ? "default" : "secondary"}>
            {titleCase(server.status)}
          </Badge>
          <span className="text-xs text-muted-foreground">{server.tool_count} tools</span>
          {server.status === "connected" ? (
            <Button
              variant="outline"
              size="sm"
              disabled={busy !== null}
              onClick={() => onLifecycle("disconnect")}
            >
              <Unplug /> Disconnect
            </Button>
          ) : (
            <Button size="sm" disabled={busy !== null} onClick={() => onLifecycle("connect")}>
              <Plug /> Connect
            </Button>
          )}
          <Button
            variant="ghost"
            size="icon-sm"
            disabled={busy !== null}
            onClick={() => onLifecycle("uninstall")}
            aria-label={`Uninstall ${server.name}`}
          >
            <Trash2 />
          </Button>
        </div>
        {server.last_error && <p className="text-sm text-destructive">{server.last_error}</p>}
        {expanded && server.status === "connected" && (
          <div className="grid gap-4 border-t pt-4 md:grid-cols-2">
            <div className="space-y-2">
              <Label>Tool</Label>
              <select
                className="h-8 w-full rounded-lg border border-input bg-background px-2.5 text-sm"
                value={toolName}
                onChange={(event) => setToolName(event.target.value)}
              >
                {tools.map((tool) => (
                  <option key={tool.name} value={tool.name}>
                    {tool.name}
                  </option>
                ))}
              </select>
              <Textarea
                data-testid="mcp-tool-call-args"
                value={argumentsText}
                onChange={(event) => setArgumentsText(event.target.value)}
                rows={6}
              />
              <Button
                data-testid="mcp-tool-call-run"
                size="sm"
                disabled={!toolName || calling}
                onClick={() => void callTool()}
              >
                {calling ? <Loader2 className="animate-spin" /> : <Play />}
                Run tool
              </Button>
            </div>
            <div className="space-y-2">
              <Label>Result</Label>
              <pre
                data-testid="mcp-tool-call-result"
                className="min-h-36 overflow-auto rounded-lg bg-muted p-3 text-xs whitespace-pre-wrap"
              >
                {result || "Run a tool to see its raw MCP result."}
              </pre>
            </div>
          </div>
        )}
      </CardContent>
    </Card>
  );
}

function Field({
  label,
  className,
  children,
}: {
  label: string;
  className?: string;
  children: React.ReactNode;
}) {
  return (
    <div className={`space-y-2 ${className ?? ""}`}>
      <Label>{label}</Label>
      {children}
    </div>
  );
}

function splitArgs(value: string): string[] {
  return value.match(/(?:[^\s"]+|"[^"]*")+/g)?.map((part) => part.replace(/^"|"$/g, "")) ?? [];
}

function titleCase(value: string): string {
  return value.charAt(0).toUpperCase() + value.slice(1);
}
