import { useEffect, useState } from "react";
import { AlertTriangle, CheckCircle2, KeyRound, Loader2, LogIn, Plug } from "lucide-react";

import type { McpHealth, McpServer, McpToolInfo } from "@/api/types";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { openOutward } from "@/lib/external-links";
import type { McpBridgeState } from "@/lib/mcp-bridge";
import { SIGN_IN_DEADLINE_MS } from "@/lib/mcp-sign-in-watch";
import {
  mcpDisplayName,
  REGISTRY_OAUTH_UNSUPPORTED_NOTICE,
  registryOauthUnsupported,
} from "@/lib/mcp-registry";
import {
  McpServerIcon,
  type PrimaryAction,
} from "@/views/connections/McpServerTable";

/** One server's sign-in, while the operator is still in the other tab. */
export interface SignInFlight {
  authorizeUrl: string;
  /** The tab could not be created — a blocked popup, or a desktop webview. */
  blocked: boolean;
  startedAtMillis: number;
  /** When the last probe answered; `null` until the first one does. */
  checkedAtMillis: number | null;
  timedOut: boolean;
}

/** The credential fields a directory install asks for, read from the directory. */
export type EnvFields =
  | { kind: "loading" }
  | { kind: "failed"; message: string }
  | { kind: "ready"; keys: string[] };

export type ToolsState =
  | { kind: "idle" }
  | { kind: "loading" }
  | { kind: "unwired" }
  | { kind: "error"; message: string }
  | { kind: "ready"; tools: McpToolInfo[] };

export interface TokenForm {
  open: boolean;
  draft: string;
  onDraft: (value: string) => void;
  onSave: () => void;
  onCancel: () => void;
}

export interface EnvForm {
  open: boolean;
  fields: EnvFields;
  draft: Record<string, string>;
  error: string | null;
  onDraft: (draft: Record<string, string>) => void;
  onSave: (keys: string[]) => void;
  onCancel: () => void;
}

interface Props {
  /** The server being connected; `null` closes the dialog. */
  server: McpServer | null;
  health?: McpHealth;
  bridge: McpBridgeState;
  canManage: boolean;
  busy: string | null;
  primary: PrimaryAction;
  flight?: SignInFlight;
  token: TokenForm;
  env: EnvForm;
  tools: ToolsState;
  onPrimary: () => void;
  onCancelSignIn: () => void;
  onCheckSignIn: () => void;
  onRetrySignIn: () => void;
  onOpenServer: () => void;
  onClose: () => void;
}

const PRIMARY_LABEL = {
  sign_in: { text: "Sign in", Icon: LogIn, testId: "mcp-connect-sign-in" },
  add_token: { text: "Add credential", Icon: KeyRound, testId: "mcp-connect-add-token" },
  rotate_env: { text: "Add credentials", Icon: KeyRound, testId: "mcp-connect-rotate-env" },
  connect: { text: "Connect", Icon: Plug, testId: "mcp-connect-connect" },
} as const;

/** Connecting one server: sign-in, credentials, and the result, in one place. */
export function McpConnectDialog({
  server,
  health,
  bridge,
  canManage,
  busy,
  primary,
  flight,
  token,
  env,
  tools,
  onPrimary,
  onCancelSignIn,
  onCheckSignIn,
  onRetrySignIn,
  onOpenServer,
  onClose,
}: Props) {
  if (!server) return null;
  const tokenOpen = token.open && canManage;
  const envOpen = env.open && canManage;
  const working = flight !== undefined || tokenOpen || envOpen;
  const connected = health?.status === "ok" && !working;
  const complaint =
    health && health.status !== "ok" && health.message.trim()
      ? health.message
      : null;
  const unreachable =
    bridge !== "absent" && server.enabled && server.reachableBy?.length === 0;
  const action = primary && canManage ? PRIMARY_LABEL[primary.kind] : null;

  return (
    <Dialog open onOpenChange={(open) => !open && onClose()}>
      <DialogContent className="sm:max-w-lg" data-testid="mcp-connect-dialog">
        <DialogHeader>
          <div className="flex items-center gap-3">
            <McpServerIcon
              iconUrl={server.iconUrl}
              name={mcpDisplayName(server)}
              className="size-10"
            />
            <div className="min-w-0">
              <DialogTitle className="truncate">{mcpDisplayName(server)}</DialogTitle>
              <DialogDescription>
                {connected ? "Connected" : "Not connected yet"}
              </DialogDescription>
            </div>
          </div>
        </DialogHeader>

        <div className="min-w-0 space-y-3">
          {connected ? (
            <div
              className="flex items-start gap-2 rounded-md border border-status-done-text/30 bg-status-done-text/10 p-3 text-sm"
              data-testid="mcp-connect-done"
            >
              <CheckCircle2 className="mt-0.5 size-4 shrink-0 text-status-done-text" />
              <div className="space-y-0.5">
                <p className="font-medium">
                  Connected · {health?.toolCount ?? 0} tool
                  {health?.toolCount === 1 ? "" : "s"}
                </p>
                <p className="text-xs text-muted-foreground">
                  {bridge === "absent"
                    ? "This build has no MCP bridge, so no agent receives these tools."
                    : "Agents with access pick up its tools on their next turn."}
                </p>
              </div>
            </div>
          ) : (
            complaint && (
              <p
                className="text-sm break-words text-muted-foreground"
                data-testid="mcp-connect-complaint"
              >
                {complaint}
              </p>
            )
          )}

          {unreachable && (
            <p
              data-testid="mcp-reachability-none"
              className="flex items-start gap-1.5 rounded-md border border-destructive/30 bg-destructive/10 px-2 py-1 text-xs font-medium text-destructive"
            >
              <AlertTriangle className="mt-0.5 size-3.5 shrink-0" />
              <span>
                No agent can reach this server — no tool grant covers{" "}
                <code className="font-mono">mcp:{server.name}</code>.
              </span>
            </p>
          )}

          {registryOauthUnsupported(server, health) && (
            <p
              className="text-xs text-muted-foreground"
              data-testid="mcp-no-credential-control"
            >
              {REGISTRY_OAUTH_UNSUPPORTED_NOTICE}
            </p>
          )}

          {flight && (
            <SignInFlightPanel
              name={server.name}
              flight={flight}
              busy={busy !== null}
              onCancel={onCancelSignIn}
              onCheck={onCheckSignIn}
              onRetry={onRetrySignIn}
            />
          )}

          {tokenOpen && (
            <form
              className="space-y-2"
              data-testid="mcp-token-inline"
              onSubmit={(event) => {
                event.preventDefault();
                token.onSave();
              }}
            >
              <Label htmlFor={`mcp-token-${server.name}`} className="text-xs">
                API token
                {server.authConfigured ? " — replaces the stored credential" : ""}
              </Label>
              <Input
                id={`mcp-token-${server.name}`}
                type="password"
                autoComplete="new-password"
                placeholder="write-only"
                autoFocus
                value={token.draft}
                onChange={(e) => token.onDraft(e.target.value)}
              />
              <div className="flex justify-end gap-2">
                <Button
                  type="button"
                  size="sm"
                  variant="ghost"
                  disabled={busy !== null}
                  onClick={token.onCancel}
                >
                  Cancel
                </Button>
                <Button
                  type="submit"
                  size="sm"
                  data-testid="mcp-token-save"
                  disabled={busy !== null || !token.draft.trim()}
                >
                  {busy === server.name ? (
                    <Loader2 className="size-4 animate-spin" />
                  ) : (
                    "Save and connect"
                  )}
                </Button>
              </div>
            </form>
          )}

          {envOpen && (
            <EnvRotation server={server} busy={busy} env={env} />
          )}

          <McpToolsList state={tools} />
        </div>

        <DialogFooter>
          {connected ? (
            <>
              <Button variant="ghost" onClick={onClose}>
                Done
              </Button>
              <Button data-testid="mcp-connect-open-server" onClick={onOpenServer}>
                Open server
              </Button>
            </>
          ) : (
            <>
              <Button variant="ghost" onClick={onClose}>
                Close
              </Button>
              {action && !working && (
                <Button
                  data-testid={action.testId}
                  disabled={busy !== null}
                  onClick={onPrimary}
                >
                  {busy === server.name ? (
                    <Loader2 className="size-4 animate-spin" />
                  ) : (
                    <action.Icon className="size-4" />
                  )}
                  {action.text}
                </Button>
              )}
            </>
          )}
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}

function EnvRotation({
  server,
  busy,
  env,
}: {
  server: McpServer;
  busy: string | null;
  env: EnvForm;
}) {
  const fields = env.fields;
  const savable = fields.kind === "ready" && fields.keys.length > 0;
  return (
    <div className="space-y-2" data-testid="mcp-env-inline">
      {fields.kind === "loading" ? (
        <p className="flex items-center gap-1 text-xs text-muted-foreground">
          <Loader2 className="size-3 animate-spin" /> Reading this server&apos;s
          credential fields…
        </p>
      ) : fields.kind === "failed" ? (
        <p className="text-xs text-destructive" data-testid="mcp-env-unavailable">
          {fields.message}
        </p>
      ) : fields.keys.length === 0 ? (
        <p className="text-xs text-muted-foreground">
          This server asks for no credentials.
        </p>
      ) : (
        fields.keys.map((key) => (
          <div key={key} className="space-y-1">
            <Label
              htmlFor={`mcp-env-${server.name}-${key}`}
              className="font-mono text-xs"
            >
              {key}
            </Label>
            <Input
              id={`mcp-env-${server.name}-${key}`}
              type="password"
              autoComplete="new-password"
              placeholder="write-only"
              value={env.draft[key] ?? ""}
              onChange={(e) => env.onDraft({ ...env.draft, [key]: e.target.value })}
            />
          </div>
        ))
      )}
      {env.error && <p className="text-xs text-destructive">{env.error}</p>}
      <div className="flex justify-end gap-2">
        <Button
          size="sm"
          variant="ghost"
          disabled={busy !== null}
          onClick={env.onCancel}
        >
          {savable ? "Cancel" : "Close"}
        </Button>
        {savable && (
          <Button
            size="sm"
            data-testid="mcp-env-save"
            disabled={busy !== null}
            onClick={() => env.onSave(fields.keys)}
          >
            {busy === server.name ? (
              <Loader2 className="size-4 animate-spin" />
            ) : (
              "Save and connect"
            )}
          </Button>
        )}
      </div>
    </div>
  );
}

function useSecondsSince(from: number | null, live: boolean): number | null {
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    if (!live) return;
    const id = window.setInterval(() => setNow(Date.now()), 1_000);
    return () => window.clearInterval(id);
  }, [live]);
  if (from === null) return null;
  return Math.max(0, Math.round((now - from) / 1000));
}

function SignInFlightPanel({
  name,
  flight,
  busy,
  onCancel,
  onCheck,
  onRetry,
}: {
  name: string;
  flight: SignInFlight;
  busy: boolean;
  onCancel: () => void;
  onCheck: () => void;
  onRetry: () => void;
}) {
  const ago = useSecondsSince(flight.checkedAtMillis, !flight.timedOut);
  const minutes = Math.round(SIGN_IN_DEADLINE_MS / 60_000);
  return (
    <div
      className="min-w-0 space-y-2 rounded-md border border-border bg-muted/30 p-3"
      data-testid="mcp-signin-flight"
    >
      {flight.timedOut ? (
        <p className="text-xs text-status-blocked-text" data-testid="mcp-signin-timed-out">
          <strong className="font-medium">
            {name} didn&apos;t report a finished sign-in within {minutes} minutes.
          </strong>{" "}
          If you completed it, check now. Otherwise start it again.
        </p>
      ) : flight.blocked ? (
        <p className="text-xs text-status-blocked-text" data-testid="mcp-signin-blocked">
          <strong className="font-medium">The sign-in tab could not be opened.</strong>{" "}
          Open this address by hand to finish:
        </p>
      ) : (
        <p className="text-xs text-muted-foreground">
          <strong className="font-medium text-foreground">
            Finish signing in to {name} in the tab that just opened.
          </strong>{" "}
          This dialog updates on its own when you come back.
        </p>
      )}
      <code
        className="block max-h-20 overflow-y-auto rounded-md border border-border bg-background px-2 py-1 font-mono text-xs break-all select-text"
        data-testid="mcp-signin-url"
      >
        {flight.authorizeUrl}
      </code>
      <div className="flex flex-wrap items-center gap-2">
        {flight.timedOut ? (
          <Button
            size="sm"
            variant="outline"
            data-testid="mcp-signin-retry"
            disabled={busy}
            onClick={onRetry}
          >
            Try again
          </Button>
        ) : (
          <Button
            size="sm"
            variant="outline"
            data-testid="mcp-signin-reopen"
            onClick={() => {
              if (!openOutward(flight.authorizeUrl)) {
                window.open(flight.authorizeUrl, "_blank", "noopener,noreferrer");
              }
            }}
          >
            Reopen sign-in
          </Button>
        )}
        <Button
          size="sm"
          variant="outline"
          data-testid="mcp-signin-check"
          onClick={onCheck}
        >
          Check now
        </Button>
        <Button
          size="sm"
          variant="ghost"
          data-testid="mcp-signin-cancel"
          onClick={onCancel}
        >
          {flight.timedOut ? "Dismiss" : "Cancel"}
        </Button>
        {!flight.timedOut && (
          <span
            className="flex items-center gap-1 text-3xs text-muted-foreground"
            data-testid="mcp-signin-checked"
          >
            <Loader2 className="size-3 animate-spin" />
            {ago === null ? "waiting for sign-in…" : `checked ${ago}s ago`}
          </span>
        )}
      </div>
    </div>
  );
}

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
        Live tool discovery isn&apos;t enabled in this build.
      </p>
    );
  }
  if (state.kind === "error") {
    return <p className="text-xs text-destructive">{state.message}</p>;
  }
  if (state.tools.length === 0) {
    return (
      <p className="text-xs text-muted-foreground">This server exposed no tools.</p>
    );
  }
  return (
    <ul className="max-h-60 space-y-1 overflow-y-auto rounded-md bg-muted/40 p-2" data-testid="mcp-tools-list">
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
