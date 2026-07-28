import { useCallback, useEffect, useRef, useState } from "react";
import { Check, KeyRound, Loader2, Plug, Save, Trash2 } from "lucide-react";
import { toast } from "sonner";

import type { OpenCompanyClient } from "@/api/client";
import { getComposioStatus, setComposioToken, type ComposioStatus } from "@/api/composio";
import { ApiError } from "@/api/types";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Skeleton } from "@/components/ui/skeleton";

interface Props {
  client: OpenCompanyClient;
  company: string | null;
}

/**
 * Per-tenant Composio connection management (issue #110, Cell D). Shows the
 * company's Composio status (granted / token configured / toolkit allowlist) and
 * a WRITE-ONLY token input: the per-tenant OAuth bearer is the entire isolation
 * lever, so it is stored and never shown again. A set/clear takes effect on the
 * agents' next turn, no restart. Hidden entirely when the feature is not in the
 * build.
 */
export function ComposioSection({ client, company }: Props) {
  const [load, setLoad] = useState<"loading" | "ready" | "unavailable">("loading");
  const [status, setStatus] = useState<ComposioStatus | null>(null);
  const [token, setToken] = useState("");
  const [busy, setBusy] = useState<"save" | "clear" | null>(null);

  const requestGeneration = useRef(0);

  const refresh = useCallback(async () => {
    const generation = ++requestGeneration.current;
    try {
      const s = await getComposioStatus(client, company);
      if (generation !== requestGeneration.current) return;
      setStatus(s);
      // Hide the whole section when the feature is not compiled into this build.
      setLoad(s.inBuild ? "ready" : "unavailable");
    } catch {
      if (generation !== requestGeneration.current) return;
      setLoad("unavailable");
    }
  }, [client, company]);

  useEffect(() => {
    setStatus(null);
    setLoad("loading");
    void refresh();
  }, [refresh]);

  async function save() {
    if (!token.trim()) return;
    setBusy("save");
    try {
      const res = await setComposioToken(client, company, token.trim());
      setStatus(res.status);
      setToken("");
      toast.success(res.note);
    } catch (err) {
      toast.error(err instanceof ApiError ? err.message : "Could not save the token.");
    } finally {
      setBusy(null);
    }
  }

  async function clear() {
    setBusy("clear");
    try {
      const res = await setComposioToken(client, company, "");
      setStatus(res.status);
      setToken("");
      toast.success("Composio token cleared.");
    } catch (err) {
      toast.error(err instanceof ApiError ? err.message : "Could not clear the token.");
    } finally {
      setBusy(null);
    }
  }

  if (load === "unavailable") return null;

  return (
    <section className="space-y-3">
      <div className="flex items-center gap-2">
        <Plug className="size-4 text-muted-foreground" />
        <h3 className="text-xs font-medium tracking-wide text-muted-foreground uppercase">
          Composio (Gmail / Slack / GitHub)
        </h3>
      </div>
      <p className="text-sm text-muted-foreground">
        Give your agents Gmail, Slack &amp; GitHub via Composio. Paste this company&apos;s Composio
        OAuth token — it is the per-tenant identity the backend bills and isolates, stored securely
        and never shown again. A change takes effect on the next turn, no restart.
      </p>

      {load === "loading" ? (
        <Skeleton className="h-32 rounded-xl" />
      ) : (
        <Card>
          <CardContent className="space-y-4 py-4">
            {status && (
              <div className="flex flex-wrap items-center gap-2">
                <Badge variant={status.granted ? "secondary" : "outline"}>
                  {status.granted ? "granted" : "not granted"}
                </Badge>
                {status.tokenConfigured ? (
                  <span className="inline-flex items-center gap-1 text-xs text-emerald-600 dark:text-emerald-400">
                    <Check className="size-3" /> token set
                  </span>
                ) : (
                  <span className="text-xs text-muted-foreground">no token yet</span>
                )}
                {status.toolkits.length > 0 && (
                  <span className="text-xs text-muted-foreground">
                    toolkits: {status.toolkits.join(", ")}
                  </span>
                )}
              </div>
            )}

            {status && !status.granted && (
              <p className="rounded-md bg-muted/40 p-2 text-xs text-muted-foreground">
                This company does not grant the <span className="font-mono">composio</span> tool
                namespace, so agents will not receive Composio tools even with a token set. Add
                <span className="font-mono"> composio</span> to the company&apos;s tool grants first.
              </p>
            )}

            <div className="space-y-1">
              <Label htmlFor="composio-token" className="text-xs">
                Composio token {status?.tokenConfigured ? "(leave blank to keep)" : ""}
              </Label>
              <Input
                id="composio-token"
                type="password"
                autoComplete="off"
                placeholder="paste the company's Composio OAuth token"
                value={token}
                onChange={(e) => setToken(e.target.value)}
              />
              {status && <p className="truncate text-xs text-muted-foreground">{status.backendUrl}</p>}
            </div>

            <div className="flex items-center gap-2">
              <Button disabled={busy !== null || !token.trim()} onClick={() => void save()}>
                {busy === "save" ? (
                  <Loader2 className="size-4 animate-spin" />
                ) : (
                  <Save className="size-4" />
                )}
                Save token
              </Button>
              {status?.tokenConfigured && (
                <Button
                  variant="outline"
                  disabled={busy !== null}
                  onClick={() => void clear()}
                >
                  {busy === "clear" ? (
                    <Loader2 className="size-4 animate-spin" />
                  ) : (
                    <Trash2 className="size-4" />
                  )}
                  Clear
                </Button>
              )}
              <KeyRound className="ml-auto size-4 text-muted-foreground" />
            </div>
          </CardContent>
        </Card>
      )}
    </section>
  );
}
