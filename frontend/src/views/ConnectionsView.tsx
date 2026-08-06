import { useCallback, useEffect, useState } from "react";
import { Check, Info, Loader2, Plug, ShieldCheck } from "lucide-react";
import { toast } from "sonner";

import { me as fetchMe } from "@/api/auth";
import type { OpenCompanyClient } from "@/api/client";
import type { ConnectionState } from "@/api/types";
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { Skeleton } from "@/components/ui/skeleton";
import {
  CONNECTION_CATEGORY_ORDER,
  CONNECTION_PROVIDERS,
  type ConnectionProvider,
} from "@/lib/connections";
import { cn } from "@/lib/utils";
import { armTourResume } from "@/tour/state";
import { InferenceSection } from "@/views/connections/InferenceSection";
import { McpServersSection } from "@/views/connections/McpServersSection";
import { ComposioSection } from "@/views/connections/ComposioSection";
import { ChannelsSection } from "./connections/ChannelsSection";

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
  // Whether this viewer may change what the company connects through (issue
  // #403). A connection belongs to the company, so changing one is an admin's
  // call; reading the page is everyone's.
  //
  // **Courtesy, not enforcement.** The host refuses every write on this page
  // with a 403 whatever this says. All hiding the controls prevents is offering
  // an operator a button that cannot work — which on this page would be a
  // particularly poor greeting, since the failure arrives only after they have
  // pasted a live credential into a form that could never submit it.
  const [canManage, setCanManage] = useState(false);

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

  useEffect(() => {
    let live = true;
    void (async () => {
      let admin = false;
      try {
        admin = (await fetchMe(client, company)).role === "admin";
      } catch {
        // No user plane on this host, or not signed in — treat as non-admin.
      }
      if (live) setCanManage(admin);
    })();
    return () => {
      live = false;
    };
  }, [client, company]);

  async function connect(p: ConnectionProvider) {
    if (busy) return;
    setBusy(p.id);
    try {
      const { url } = await client.startConnection(p.id, company);
      // Unlike the MCP sign-in below (which opens a tab and survives), this
      // navigates the whole document away — taking the product tour's
      // in-memory step state with it. Arm a resume marker so the operator comes
      // back to the stop they left instead of the tour restarting from step 1
      // (issue #300). No-op when no tour is running, and deliberately after the
      // start call succeeds: a provider that isn't configured 400s here and
      // never navigates, so it must not leave a marker behind.
      armTourResume(company);
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
  // `attested` is a property of the *instance*, not of one provider: it means
  // this pod carries a platform-minted identity, so every connection is the
  // platform's to run. One row reporting it is therefore enough to say so once
  // at the top, rather than only in per-tile copy (issue #319).
  const platformManaged =
    load === "ready" && Object.values(states).some((s) => s.credentialSource === "attested");

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

        {platformManaged && (
          <Alert data-testid="connections-platform-managed">
            <ShieldCheck className="size-4" />
            <AlertTitle>Connections are managed by the platform</AlertTitle>
            <AlertDescription>
              This instance signs in with its own platform identity, so there is no provider key to
              register here and nothing stored on this instance. Connect a provider from the
              platform and it shows up here.
            </AlertDescription>
          </Alert>
        )}

        {!canManage && (
          <Alert data-testid="connections-read-only">
            <Info className="size-4" />
            <AlertTitle>Only an admin can change what this company connects through</AlertTitle>
            <AlertDescription>
              A connection belongs to the company — it is the account your agents act through — so
              an admin manages it. You can see everything that is wired here; ask an admin to add,
              change or remove one.
            </AlertDescription>
          </Alert>
        )}

        <McpServersSection client={client} company={company} canManage={canManage} />

        <InferenceSection client={client} company={company} canManage={canManage} />

        <ComposioSection client={client} company={company} canManage={canManage} />

        <ChannelsSection client={client} company={company} canManage={canManage} />

        {load === "ready" && (
          <Alert data-testid="connections-catalog-advisory">
            <Info className="size-4" />
            <AlertTitle>Agents reach these providers through Composio</AlertTitle>
            <AlertDescription>
              Connecting below records the account against this company, but the tool belt your
              agents actually run on is wired in the Composio section above — that is where a
              connection becomes something an agent can do. Connect Gmail, GitHub or Slack there
              first (issue #396).
            </AlertDescription>
          </Alert>
        )}

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
                      platformManaged={platformManaged}
                      disabled={load === "unavailable" || !canManage}
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

/**
 * One provider tile.
 *
 * The unconnected foot of the card is a tri-state driven by the host's
 * `credentialSource` (issue #319) — the same three tiers the Composio section
 * above already reports, worded the same way on purpose:
 *
 * - `static` — a Connect can succeed here, either through a token this company
 *   already holds or through this host's own registered provider application
 *   (the self-hosted hatch). Unchanged: the Connect button, exactly as today.
 * - `attested` — this instance carries a platform identity, so connections are
 *   the platform's to run and no provider credential is (or should be) present
 *   locally. A local Connect on such a host could only ever 400, so the button
 *   is replaced by "Managed by the platform" and that failure becomes
 *   unreachable from the console by construction.
 * - `none` — nothing can complete a handshake here; the tile says so instead of
 *   offering a button that fails.
 *
 * A host that predates the field sends no `credentialSource`; that falls back
 * to the Connect button so an older host is never silently disabled.
 *
 * `platformManaged` covers the tiles the host said nothing about. `/connections`
 * only answers for providers the company's manifest declares, but this grid
 * renders the whole catalog — so on a hosted instance the undeclared tiles would
 * otherwise keep a Connect button sitting under a banner that says connections
 * are the platform's, and clicking it would 400. `attested` is a property of the
 * *instance*, not of one provider, so it is safe to apply to every tile. `none`
 * is NOT: it is provider-specific, and a host can hold a provider app for a
 * provider the manifest never declared, where Connect genuinely works.
 */
function ConnectionCard({
  provider,
  state,
  platformManaged,
  disabled,
  busy,
  onConnect,
  onDisconnect,
}: {
  provider: ConnectionProvider;
  state?: ConnectionState;
  platformManaged: boolean;
  disabled: boolean;
  busy: boolean;
  onConnect: () => void;
  onDisconnect: () => void;
}) {
  const connected = Boolean(state?.connected);
  const source = state?.credentialSource;
  const managedByPlatform = source === "attested" || (platformManaged && source === undefined);
  const noRoute = source === "none";
  // Which namespace actually backs this connection. `native` alone means the
  // credential sits in the host's catalog, which no agent tool reads yet — worth
  // distinguishing from a Composio connection, which is a live capability.
  const via = state?.via ?? [];
  const nativeOnly = connected && via.length > 0 && !via.includes("composio");
  const unverified = state?.unverified === true;
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
            {connected && via.length > 0 && (
              <p className="mt-0.5 text-xs text-muted-foreground" data-testid={`connection-via-${provider.id}`}>
                via {via.join(" + ")}
                {nativeOnly && " — stored here; agents use the Composio connection"}
              </p>
            )}
            {!connected && unverified && (
              <p className="mt-0.5 text-xs text-amber-600 dark:text-amber-400">
                Couldn&apos;t check the Composio connection — state unknown.
              </p>
            )}
          </div>
        </div>
        <div className="mt-auto">
          {connected ? (
            <Button variant="outline" size="sm" className="w-full" disabled={busy} onClick={onDisconnect}>
              {busy ? <Loader2 className="size-4 animate-spin" /> : null}
              Disconnect
            </Button>
          ) : managedByPlatform ? (
            <p
              className="inline-flex items-center gap-1 text-xs text-emerald-600 dark:text-emerald-400"
              data-testid={`connection-managed-${provider.id}`}
            >
              <ShieldCheck className="size-3" /> Managed by the platform — nothing to set up here
            </p>
          ) : noRoute ? (
            <p
              className="text-xs text-muted-foreground"
              data-testid={`connection-unavailable-${provider.id}`}
            >
              Not available on this host.
            </p>
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
