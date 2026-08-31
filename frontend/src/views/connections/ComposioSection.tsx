import { useCallback, useEffect, useRef, useState } from "react";
import { Check, KeyRound, Loader2, Plug, Save, ShieldCheck, Trash2 } from "lucide-react";
import { toast } from "sonner";

import type { OpenCompanyClient } from "@/api/client";
import { getComposioStatus, setComposioToken, type ComposioStatus } from "@/api/composio";
import { ApiError } from "@/api/types";
import { grantStanding } from "@/lib/provider-grid";
import { classifyLoadFailure } from "@/lib/section-load";
import { SectionUnreachable } from "@/views/connections/SectionUnreachable";
import { GrantNamespace } from "@/components/grant-namespace";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Skeleton } from "@/components/ui/skeleton";

interface Props {
  client: OpenCompanyClient;
  company: string | null;
  /**
   * Whether this viewer may change what the company connects through (issue
   * #403) — the credential its agents present, and which provider accounts
   * they act through.
   *
   * **Courtesy, not enforcement.** The host refuses both writes with a 403
   * whatever this says. What it prevents is offering a token field whose Save is
   * refused only after the operator has already pasted a live credential into
   * it.
   */
  canManage: boolean;
  /**
   * Called after the stored token changes.
   *
   * The provider grid's status and routing are downstream of which credential
   * this company reaches Composio with — setting or clearing a token here flips
   * `credentialSource`, and every tile's route with it. Without this the grid
   * would keep rendering the old answer while this section reported the new one:
   * the same two-surfaces-disagreeing failure #582 is about, arriving through
   * the credential rather than through the connection list.
   */
  onChanged: () => void;
}

/**
 * Which credential this company reaches Composio with (issue #110, Cell D).
 *
 * **Only that.** This section used to carry a second thing — a per-provider
 * "Sign in per provider" grid — and that grid was one of the two provider lists
 * issue #582 collapsed. The page now has a single grid, `ProvidersSection`,
 * built from the reconciled `GET …/connections` status; what is left here is the
 * credential layer, which is genuinely independent of it:
 *
 * - `attested` (hosted) — the instance already holds a platform identity, so
 *   there is nothing to paste and nothing stored here. A company that wants to
 *   use its OWN Composio account can still override.
 * - `company` (issue #586) — this company's own TinyHumans credential, set by
 *   its admin. Composio is brokered through it, so there is nothing to paste
 *   here either. The override still exists for a company that wants its own
 *   Composio account.
 * - `static` — a Composio token this company pasted, or a static instance key.
 * - `none` — no credential can be obtained, so there is nothing to authorize
 *   against and agents get no Composio tools.
 *
 * The pasted token is WRITE-ONLY: stored and never shown again. A set/clear
 * takes effect on the agents' next turn, no restart. Hidden entirely when the
 * feature is not in the build.
 */
export function ComposioSection({ client, company, canManage, onChanged }: Props) {
  const [load, setLoad] = useState<"loading" | "ready" | "unavailable" | "error">("loading");
  const [status, setStatus] = useState<ComposioStatus | null>(null);
  const [token, setToken] = useState("");
  const [busy, setBusy] = useState<"save" | "clear" | null>(null);
  // Only meaningful in the credentialled states, where the paste card is an
  // override rather than the way in.
  const [showOverride, setShowOverride] = useState(false);

  const requestGeneration = useRef(0);

  const refresh = useCallback(async () => {
    const generation = ++requestGeneration.current;
    try {
      const s = await getComposioStatus(client, company);
      if (generation !== requestGeneration.current) return;
      setStatus(s);
      // Hide the whole section when the feature is not compiled into this build.
      setLoad(s.inBuild ? "ready" : "unavailable");
    } catch (err) {
      if (generation !== requestGeneration.current) return;
      // A 404 is a host with no Composio surface — hide it. Anything else is a
      // host that could not answer; keep the section rather than vanishing
      // (issue #1470).
      setLoad(classifyLoadFailure(err));
    }
  }, [client, company]);

  useEffect(() => {
    setStatus(null);
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
      onChanged();
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
      // Clearing an override falls back to whatever tier remains — the status
      // the host just returned says which, and the grid re-probes for itself.
      toast.success(res.note);
      onChanged();
    } catch (err) {
      toast.error(err instanceof ApiError ? err.message : "Could not clear the token.");
    } finally {
      setBusy(null);
    }
  }

  if (load === "unavailable") return null;

  const attested = status?.credentialSource === "attested";
  // The company's own key already authorizes Composio (issue #586), so this
  // reads like `attested` everywhere the question is "is there anything to
  // paste?" — the difference is whose identity it is, which the copy states.
  const companyKey = status?.credentialSource === "company";
  const byoToken = status?.credentialSource === "static";
  // In the attested state the paste card is a deliberate override; everywhere
  // else it is the only way to connect, so it is always on screen.
  // Issue #403: the credential card is an admin's. A member still sees the
  // status line above ("token set" / "linked via cluster identity"), which is
  // what tells them why their agents can reach Gmail; what they do not get is a
  // field that invites them to paste a credential the host will refuse.
  // A company already brokered through its own key is in the same position as an
  // attested one: the paste card is a deliberate override, not the way in.
  const credentialed = attested || companyKey;
  const showTokenCard = canManage && (!credentialed || showOverride || byoToken);
  // The composio-grant tri-state, narrowed the same way `ProvidersSection` does
  // (issue #1478): `undefined` reads as "unknown", never as "not granted", so
  // this badge and the grid a few inches below it cannot disagree on the same
  // field. `status` is non-null wherever this is read below.
  const grant = grantStanding(status?.granted);

  return (
    <section className="space-y-3">
      <div className="flex items-center gap-2">
        <Plug className="size-4 text-muted-foreground" />
        <h2 className="text-xs font-medium tracking-wide text-muted-foreground uppercase">
          Composio credential
        </h2>
      </div>
      <p className="text-sm text-muted-foreground">
        {!canManage
          ? "Your teammates reach Gmail, Slack & GitHub through Composio. Which account they act through belongs to the company, so an admin manages it — this is what is wired today."
          : attested
            ? "Your teammates reach providers through Composio. This company is linked through this instance's own cluster identity — there is no key to copy and nothing stored here. Connect providers in the grid below."
            : companyKey
              ? "Your teammates reach providers through Composio. This company's own TinyHumans credential already authorizes it — there is no separate Composio token to paste and no provider app to register. Connect providers in the grid below; every teammate in the company can then use what you connect."
              : "Your teammates reach providers through Composio. Paste this company's Composio OAuth token — it is the identity the backend bills and isolates, stored securely and never shown again — then connect providers in the grid below. A change takes effect on the next turn, no restart."}
      </p>

      {load === "loading" ? (
        <Skeleton className="h-32 rounded-xl" />
      ) : load === "error" ? (
        <SectionUnreachable label="Couldn't read this company's Composio credential" />
      ) : (
        <>
          {status && (
            <div className="flex flex-wrap items-center gap-2">
              <Badge variant={grant === "granted" ? "secondary" : "outline"}>
                {grant === "granted"
                  ? "granted"
                  : grant === "not-granted"
                    ? "not granted"
                    : "grant unknown"}
              </Badge>
              {attested ? (
                <span className="inline-flex items-center gap-1 text-xs text-status-done-text">
                  <ShieldCheck className="size-3" /> Linked via cluster identity — nothing stored
                </span>
              ) : companyKey ? (
                <span className="inline-flex items-center gap-1 text-xs text-status-done-text">
                  <ShieldCheck className="size-3" /> Linked via this company&apos;s own credential
                </span>
              ) : byoToken ? (
                <span className="inline-flex items-center gap-1 text-xs text-status-done-text">
                  <Check className="size-3" /> token set
                </span>
              ) : (
                <span className="text-xs text-muted-foreground">not connected</span>
              )}
            </div>
          )}

          {/* Fires only on an explicit not-granted, never on an unchecked grant
              (issue #1478): telling an operator to widen a grant that may
              already be set, off a field that was never read, is the same false
              confidence the badge above used to show. */}
          {grant === "not-granted" && (
            <GrantNamespace
              client={client}
              company={company}
              namespace="composio"
              explanation="Teammates will not receive Composio tools even once connected."
              canManage={canManage}
              onGranted={async () => {
                await refresh();
                onChanged();
              }}
              testId="composio-not-granted"
            />
          )}

          {/* Gated on `credentialed`, not `attested`: a company brokered through
              its own TinyHumans key is equally "already credentialled", so it
              equally needs a way back to the BYO card. Gating this on `attested`
              alone left a company-key admin with the paste card hidden and no
              control to reveal it — the override became unreachable. */}
          {canManage && credentialed && !showTokenCard && (
            <Button variant="outline" size="sm" onClick={() => setShowOverride(true)}>
              <KeyRound className="size-4" />
              Use your own Composio account instead
            </Button>
          )}

          {showTokenCard && (
            <Card>
              <CardContent className="space-y-4">
                {/* The explainer has to name what the token would displace,
                    and that differs by tier: an attested company falls back to
                    the pod's cluster identity, a company-key one falls back to
                    its own credential. Saying "cluster identity" to the latter
                    would describe a fallback it does not have. */}
                {companyKey ? (
                  <p className="rounded-md bg-muted/40 p-2 text-xs text-muted-foreground">
                    Optional. This company&apos;s own TinyHumans credential already authorizes
                    Composio. A token set here replaces it for Composio only — use it when the
                    company has a separate Composio account. Clear it to go back to the company
                    credential.
                  </p>
                ) : attested ? (
                  <p className="rounded-md bg-muted/40 p-2 text-xs text-muted-foreground">
                    Optional. A token set here overrides the instance identity for this company only
                    — use it when the company has its own Composio account. Clear it to go back to
                    the cluster identity.
                  </p>
                ) : null}
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
