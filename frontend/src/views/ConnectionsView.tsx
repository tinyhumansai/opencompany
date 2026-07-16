import { useCallback, useEffect, useState } from "react";
import { Check, Info, Loader2, Plug } from "lucide-react";
import { toast } from "sonner";

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
