import { useCallback, useEffect, useRef, useState } from "react";
import { Check, KeyRound, Loader2, LogIn, Plug, Save, ShieldCheck, Trash2 } from "lucide-react";
import { toast } from "sonner";

import type { OpenCompanyClient } from "@/api/client";
import {
  getComposioStatus,
  listComposioConnections,
  setComposioToken,
  startComposioAuthorize,
  type ComposioStatus,
} from "@/api/composio";
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

/** Friendly display labels for the common toolkits; slug-cased fallback otherwise. */
const TOOLKIT_LABELS: Record<string, string> = {
  gmail: "Gmail",
  slack: "Slack",
  github: "GitHub",
  googlecalendar: "Google Calendar",
  notion: "Notion",
  linear: "Linear",
};

function toolkitLabel(slug: string): string {
  return TOOLKIT_LABELS[slug.toLowerCase()] ?? slug.charAt(0).toUpperCase() + slug.slice(1);
}

/**
 * Per-tenant Composio connection management (issue #110, Cell D).
 *
 * Two layers, and they are independent:
 *
 * 1. **Which credential this company reaches Composio with**, reported as
 *    `credentialSource`:
 *    - `attested` (hosted) — the instance already holds a platform identity, so
 *      there is nothing to paste and nothing stored here. A company that wants
 *      to use its OWN Composio account can still override.
 *    - `static` — a token this company pasted, or a static instance key.
 *    - `none` — no credential can be obtained, so there is nothing to authorize
 *      against and agents get no Composio tools.
 * 2. **Which providers are connected**, via a per-provider OAuth "Sign in" list
 *    driven by the granted toolkits. Composio runs the hosted OAuth; we open it
 *    in a tab and poll until the toolkit reports connected.
 *
 * The pasted token is WRITE-ONLY: stored and never shown again. A set/clear
 * takes effect on the agents' next turn, no restart. Hidden entirely when the
 * feature is not in the build.
 */
export function ComposioSection({ client, company }: Props) {
  const [load, setLoad] = useState<"loading" | "ready" | "unavailable">("loading");
  const [status, setStatus] = useState<ComposioStatus | null>(null);
  const [token, setToken] = useState("");
  const [busy, setBusy] = useState<"save" | "clear" | null>(null);
  // toolkit slug -> connected. Absent = unknown / not yet fetched.
  const [connected, setConnected] = useState<Record<string, boolean>>({});
  // toolkit slug currently mid-sign-in (open tab + poll).
  const [signingIn, setSigningIn] = useState<string | null>(null);
  // Only meaningful in the attested state, where the paste card is an override
  // rather than the way in.
  const [showOverride, setShowOverride] = useState(false);

  const requestGeneration = useRef(0);
  const pollTimers = useRef<Record<string, number>>({});

  // Clear any in-flight poll timers on unmount / company switch.
  useEffect(() => {
    const timers = pollTimers.current;
    return () => {
      Object.values(timers).forEach((id) => window.clearTimeout(id));
      pollTimers.current = {};
    };
  }, [company]);

  const refreshConnections = useCallback(
    async (s: ComposioStatus) => {
      // Listing connections needs *a* credential — any tier. With none there is
      // nothing to authorize against, and the call 409s.
      if (s.credentialSource === "none" || s.toolkits.length === 0) {
        setConnected({});
        return;
      }
      try {
        const rows = await listComposioConnections(client, company);
        setConnected(Object.fromEntries(rows.map((r) => [r.toolkit.toLowerCase(), r.connected])));
      } catch {
        // Non-fatal: leave the provider rows in their "sign in" state.
        setConnected({});
      }
    },
    [client, company],
  );

  const refresh = useCallback(async () => {
    const generation = ++requestGeneration.current;
    try {
      const s = await getComposioStatus(client, company);
      if (generation !== requestGeneration.current) return;
      setStatus(s);
      // Hide the whole section when the feature is not compiled into this build.
      setLoad(s.inBuild ? "ready" : "unavailable");
      if (s.inBuild) void refreshConnections(s);
    } catch {
      if (generation !== requestGeneration.current) return;
      setLoad("unavailable");
    }
  }, [client, company, refreshConnections]);

  useEffect(() => {
    setStatus(null);
    setConnected({});
    setShowOverride(false);
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
      void refreshConnections(res.status);
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
      setConnected({});
      // Clearing an override falls back to whatever tier remains — re-probe
      // rather than assuming there is no credential left.
      toast.success("Composio token cleared.");
      void refreshConnections(res.status);
    } catch (err) {
      toast.error(err instanceof ApiError ? err.message : "Could not clear the token.");
    } finally {
      setBusy(null);
    }
  }

  // Per-provider OAuth: open Composio's hosted connect URL in a new tab, then
  // poll the connection list every 2s up to ~2 minutes until this toolkit flips
  // to connected (Composio completes the OAuth on its side — no local callback).
  async function signIn(toolkit: string) {
    // Guard the shared flag AND a poll already in flight for this toolkit.
    if (signingIn || pollTimers.current[toolkit] !== undefined) return;
    setSigningIn(toolkit);
    const label = toolkitLabel(toolkit);
    try {
      const { connectUrl } = await startComposioAuthorize(client, company, toolkit);
      window.open(connectUrl, "_blank", "noopener,noreferrer");
      toast.message(`Complete ${label} sign-in in the new tab.`);
      const deadline = Date.now() + 120_000;
      const poll = async () => {
        delete pollTimers.current[toolkit];
        if (Date.now() > deadline) {
          setSigningIn((t) => (t === toolkit ? null : t));
          toast.message(`${label} sign-in timed out. Try again if it didn't complete.`);
          return;
        }
        try {
          const rows = await listComposioConnections(client, company);
          const map = Object.fromEntries(rows.map((r) => [r.toolkit.toLowerCase(), r.connected]));
          setConnected(map);
          if (map[toolkit.toLowerCase()]) {
            setSigningIn((t) => (t === toolkit ? null : t));
            toast.success(`Connected ${label}.`);
            return;
          }
        } catch {
          // Ignore transient probe errors while the operator finishes sign-in.
        }
        pollTimers.current[toolkit] = window.setTimeout(() => void poll(), 2_000);
      };
      pollTimers.current[toolkit] = window.setTimeout(() => void poll(), 2_000);
    } catch (err) {
      setSigningIn((t) => (t === toolkit ? null : t));
      toast.error(err instanceof ApiError ? err.message : `Couldn't start ${label} sign-in.`);
    }
  }

  if (load === "unavailable") return null;

  const attested = status?.credentialSource === "attested";
  const byoToken = status?.credentialSource === "static";
  // No credential of any tier: there is nothing to authorize a provider against.
  const noCredential = status?.credentialSource === "none";
  const showProviders = load === "ready" && status !== null && status.toolkits.length > 0;
  // In the attested state the paste card is a deliberate override; everywhere
  // else it is the only way to connect, so it is always on screen.
  const showTokenCard = !attested || showOverride || byoToken;

  return (
    <section className="space-y-3">
      <div className="flex items-center gap-2">
        <Plug className="size-4 text-muted-foreground" />
        <h3 className="text-xs font-medium tracking-wide text-muted-foreground uppercase">
          Composio (Gmail / Slack / GitHub)
        </h3>
      </div>
      <p className="text-sm text-muted-foreground">
        {attested
          ? "Give your agents Gmail, Slack & GitHub via Composio. This company is linked through this instance's own cluster identity — there is no key to copy and nothing stored here. Sign in per provider below."
          : "Give your agents Gmail, Slack & GitHub via Composio. Paste this company's Composio OAuth token — it is the identity the backend bills and isolates, stored securely and never shown again — then sign in per provider below. A change takes effect on the next turn, no restart."}
      </p>

      {load === "loading" ? (
        <Skeleton className="h-32 rounded-xl" />
      ) : (
        <>
          {status && (
            <div className="flex flex-wrap items-center gap-2">
              <Badge variant={status.granted ? "secondary" : "outline"}>
                {status.granted ? "granted" : "not granted"}
              </Badge>
              {attested ? (
                <span className="inline-flex items-center gap-1 text-xs text-emerald-600 dark:text-emerald-400">
                  <ShieldCheck className="size-3" /> Linked via cluster identity — nothing stored
                </span>
              ) : byoToken ? (
                <span className="inline-flex items-center gap-1 text-xs text-emerald-600 dark:text-emerald-400">
                  <Check className="size-3" /> token set
                </span>
              ) : (
                <span className="text-xs text-muted-foreground">not connected</span>
              )}
            </div>
          )}

          {status && !status.granted && (
            <p className="rounded-md bg-muted/40 p-2 text-xs text-muted-foreground">
              This company does not grant the <span className="font-mono">composio</span> tool
              namespace, so agents will not receive Composio tools even once connected. Add
              <span className="font-mono"> composio</span> to the company&apos;s tool grants first.
            </p>
          )}

          {showProviders && status && (
            <Card>
              <CardContent className="space-y-2 py-4">
                <p className="text-xs font-medium text-muted-foreground">Sign in per provider</p>
                {noCredential && (
                  <p className="rounded-md bg-muted/40 p-2 text-xs text-muted-foreground">
                    No Composio credential is available for this company yet, so there is nothing to
                    authorize against. Paste a token below first.
                  </p>
                )}
                <ul className="divide-y divide-border">
                  {status.toolkits.map((toolkit) => {
                    const isConnected = connected[toolkit.toLowerCase()] === true;
                    const isSigningIn = signingIn === toolkit;
                    return (
                      <li key={toolkit} className="flex items-center justify-between py-2">
                        <span className="text-sm">{toolkitLabel(toolkit)}</span>
                        {isConnected ? (
                          <span className="inline-flex items-center gap-1 text-xs text-emerald-600 dark:text-emerald-400">
                            <Check className="size-3" /> connected
                          </span>
                        ) : (
                          <Button
                            size="sm"
                            variant="outline"
                            disabled={signingIn !== null || noCredential}
                            onClick={() => void signIn(toolkit)}
                          >
                            {isSigningIn ? (
                              <Loader2 className="size-4 animate-spin" />
                            ) : (
                              <LogIn className="size-4" />
                            )}
                            Sign in
                          </Button>
                        )}
                      </li>
                    );
                  })}
                </ul>
              </CardContent>
            </Card>
          )}

          {attested && !showTokenCard && (
            <Button variant="outline" size="sm" onClick={() => setShowOverride(true)}>
              <KeyRound className="size-4" />
              Use your own Composio account instead
            </Button>
          )}

          {showTokenCard && (
            <Card>
              <CardContent className="space-y-4 py-4">
                {attested && (
                  <p className="rounded-md bg-muted/40 p-2 text-xs text-muted-foreground">
                    Optional. A token set here overrides the instance identity for this company only
                    — use it when the company has its own Composio account. Clear it to go back to
                    the cluster identity.
                  </p>
                )}
                <div className="space-y-1">
                  <Label htmlFor="composio-token" className="text-xs">
                    Composio token {byoToken ? "— set (paste a new value to rotate)" : ""}
                  </Label>
                  <Input
                    id="composio-token"
                    type="password"
                    autoComplete="off"
                    placeholder="paste the company's Composio OAuth token"
                    value={token}
                    onChange={(e) => setToken(e.target.value)}
                  />
                  {status && (
                    <p className="truncate text-xs text-muted-foreground">{status.backendUrl}</p>
                  )}
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
                  {byoToken && (
                    <Button variant="outline" disabled={busy !== null} onClick={() => void clear()}>
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
        </>
      )}
    </section>
  );
}
