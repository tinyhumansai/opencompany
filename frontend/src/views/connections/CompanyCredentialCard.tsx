import { useCallback, useEffect, useRef, useState } from "react";
import { KeyRound, Loader2, Save, ShieldCheck, Trash2, Wallet } from "lucide-react";
import { toast } from "sonner";

import type { OpenCompanyClient } from "@/api/client";
import {
  getCompanyCredential,
  setCompanyCredential,
  type CompanyCredentialStatus,
} from "@/api/credential";
import { ApiError } from "@/api/types";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Skeleton } from "@/components/ui/skeleton";

interface Props {
  client: OpenCompanyClient;
  company: string | null;
  /**
   * Whether the signed-in operator may change the company's credential. Admins
   * only (issue #403's discipline): this key is the identity every one of the
   * company's agents presents, and it is the company's wallet. A member still
   * sees the status — that is what tells them why their agents can reach Gmail
   * — but not a field inviting them to paste a credential the host will refuse.
   */
  canManage: boolean;
  /** Called after a successful write, so sibling sections re-read their status. */
  onChanged?: () => void;
}

/**
 * The company's own TinyHumans credential (issue #586).
 *
 * One key, set once by the admin, and every surface wired to it presents it —
 * Composio today, whatever is wired next. This is what replaces
 * per-tenant provider apps and per-surface tokens: a company with a key set can
 * connect a provider without registering anything, and a provider connected
 * that way belongs to the company rather than to the member who connected it.
 *
 * Sits **above** the Composio section deliberately. It is the general answer;
 * the Composio token below it is the escape hatch for a company that insists on
 * its own Composio account.
 *
 * Write-only: the key is sent and never returned, so the field is always empty
 * on load and "set" is reported by a flag, never by a masked value we would have
 * had to receive.
 */
export function CompanyCredentialCard({ client, company, canManage, onChanged }: Props) {
  const [load, setLoad] = useState<"loading" | "ready" | "unavailable" | "error">("loading");
  const [error, setError] = useState<string | null>(null);
  const [status, setStatus] = useState<CompanyCredentialStatus | null>(null);
  const [key, setKey] = useState("");
  const [busy, setBusy] = useState<"save" | "clear" | null>(null);

  // Discards the result of a request whose company is no longer the one on
  // screen. `refresh` re-runs when `company` changes, but nothing cancels the
  // request already in flight, so two responses can land out of order and the
  // later `setStatus` be the *earlier* company's. On this card that is
  // decision-shaped rather than cosmetic: an admin who reads "configured" for a
  // company that in fact has no key will not set one. Same generation guard
  // `ComposioSection` uses next door.
  const requestGeneration = useRef(0);

  const refresh = useCallback(async () => {
    const generation = ++requestGeneration.current;
    try {
      const next = await getCompanyCredential(client, company);
      if (generation !== requestGeneration.current) return;
      setStatus(next);
      setError(null);
      setLoad("ready");
    } catch (err) {
      if (generation !== requestGeneration.current) return;
      // Only a genuinely absent route hides the card. A host predating this
      // plane 404s, and rendering nothing is right there — the console still
      // works, this company just has no such surface.
      //
      // Anything else must NOT be swallowed. The status route now surfaces a
      // secret-store read failure as a 5xx rather than reporting a confident
      // "not configured" (see `company_key::resolve`), and hiding the card on
      // that would throw away the very distinction that fix exists to draw:
      // the operator would see no credential UI at all and conclude the feature
      // is absent, rather than that the host could not answer.
      if (err instanceof ApiError && err.status === 404) {
        setLoad("unavailable");
        return;
      }
      setError(err instanceof ApiError ? err.message : "Couldn't read the company credential.");
      setLoad("error");
    }
  }, [client, company]);

  useEffect(() => {
    setLoad("loading");
    setKey("");
    void refresh();
  }, [refresh]);

  async function write(value: string, mode: "save" | "clear") {
    setBusy(mode);
    const generation = ++requestGeneration.current;
    try {
      const result = await setCompanyCredential(client, company, value);
      // The toast is unconditional: the write really did land, and an admin who
      // navigated away mid-request is still owed that fact. What is conditional
      // is the *state* — `result.status` describes the company that was on
      // screen when the write started, so painting it after a switch would put
      // one company's credential state under another's name.
      toast.success(mode === "save" ? "Credential saved." : "Credential cleared.", {
        description: result.note,
      });
      if (generation !== requestGeneration.current) return;
      setStatus(result.status);
      setKey("");
      onChanged?.();
    } catch (err) {
      // Show the host's own reason when it sent one — an admin-only refusal or a
      // store failure says something specific, and replacing it with a generic
      // "couldn't save" throws away the only actionable part.
      toast.error(
        err instanceof ApiError
          ? err.message
          : mode === "save"
          ? "Couldn't save the credential."
          : "Couldn't clear the credential.",
      );
    } finally {
      setBusy(null);
    }
  }

  if (load === "unavailable") return null;

  const configured = status?.configured === true;
  // No company key, but the instance carries an identity of its own — calls
  // still work, they are just attributed to the instance rather than to this
  // company. Worth saying, because "not configured" alone reads as broken.
  const instanceFallback =
    !configured && (status?.source === "attested" || status?.source === "static");

  return (
    <section className="space-y-3">
      <div className="flex items-center gap-2">
        <Wallet className="size-4 text-muted-foreground" />
        <h2 className="text-xs font-medium tracking-wide text-muted-foreground uppercase">
          Company credential
        </h2>
        <span className="text-xs text-muted-foreground">
          for connecting providers — not the model key
        </span>
      </div>

      {load === "loading" ? (
        <Skeleton className="h-32 rounded-xl" />
      ) : load === "error" ? (
        // Say the host could not answer. Rendering the card's "Not set" state
        // here would be a confident claim built on no data.
        <Card>
          <CardContent>
            <p className="text-xs text-muted-foreground">
              {error ?? "Couldn't read the company credential."}{" "}
              This is not the same as having none set — the host could not answer, so the current
              credential is unknown.
            </p>
          </CardContent>
        </Card>
      ) : (
        <Card>
          <CardContent className="space-y-4">
            <div className="flex flex-wrap items-center gap-2">
              {configured ? (
                <span className="inline-flex items-center gap-1 text-xs text-status-done-text">
                  <ShieldCheck className="size-3" /> Set — providers connect as this company
                </span>
              ) : instanceFallback ? (
                <span className="text-xs text-muted-foreground">
                  Not set — using this instance&apos;s identity
                </span>
              ) : (
                <span className="text-xs text-muted-foreground">Not set</span>
              )}
            </div>

            {/* The host words the consequence, so the console cannot drift from
                what the host actually does. */}
            {status && (
              <p className="rounded-md bg-muted/40 p-2 text-xs text-muted-foreground">
                {status.notice}
              </p>
            )}

            {canManage && (
              <>
                <div className="space-y-1">
                  <Label htmlFor="company-credential" className="text-xs">
                    TinyHumans account key{" "}
                    {configured ? "— set (paste a new value to rotate)" : ""}
                  </Label>
                  <Input
                    id="company-credential"
                    type="password"
                    autoComplete="off"
                    placeholder="paste this company's TinyHumans account key"
                    value={key}
                    onChange={(e) => setKey(e.target.value)}
                  />
                </div>

                <div className="flex items-center gap-2">
                  <Button
                    disabled={busy !== null || !key.trim()}
                    onClick={() => void write(key.trim(), "save")}
                  >
                    {busy === "save" ? (
                      <Loader2 className="size-4 animate-spin" />
                    ) : (
                      <Save className="size-4" />
                    )}
                    {configured ? "Rotate key" : "Save key"}
                  </Button>
                  {configured && (
                    <Button
                      variant="outline"
                      disabled={busy !== null}
                      onClick={() => void write("", "clear")}
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
              </>
            )}
          </CardContent>
        </Card>
      )}
    </section>
  );
}
