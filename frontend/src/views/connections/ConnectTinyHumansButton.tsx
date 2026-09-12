import { useCallback, useEffect, useRef, useState } from "react";
import { Loader2, Sparkles } from "lucide-react";
import { toast } from "sonner";

import type { OpenCompanyClient } from "@/api/client";
import { finishCredentialLink, startCredentialLink } from "@/api/credential";
import { ApiError } from "@/api/types";
import { Button } from "@/components/ui/button";
import { takeKeyLink, takeKeyLinkRefusal } from "@/lib/pending-key-link";

interface Props {
  client: OpenCompanyClient;
  company: string | null;
  /** Whether this host can complete a grant at all (`hubLink` on the status). */
  available: boolean;
  /** Whether the signed-in operator may change the company's credential. */
  canManage: boolean;
  /** Whether a credential is already stored, which changes the verb. */
  configured: boolean;
  /** Called after a successful connection, so the page re-reads its status. */
  onConnected?: () => void;
  /**
   * Whether to render the sentence under the button.
   *
   * True everywhere the button stands in a column of prose that does not
   * already explain what connecting buys. False on the Account page, whose
   * header card carries that sentence as its own sub-line — printing it again
   * two inches lower is the duplication that page's pass exists to remove, and
   * a paragraph inside a `justify-between` header row is not a shape this
   * component can make look right anyway.
   */
  hint?: boolean;
}

/**
 * "Connect TinyHumans" — the whole key flow, as one button.
 *
 * Used by both pages that used to ask for a pasted key, so the two cannot drift
 * into different versions of the same errand. Clicking it starts a PKCE grant on
 * the host, navigates to the hub, and — on the way back — the same component,
 * freshly mounted, claims the code and finishes the exchange.
 *
 * ## Why the return leg lives here rather than in `App`
 *
 * `App` captures the code off the URL, because that is where a landing URL is
 * read and stripped. It does not *redeem* it: a grant is not a session
 * credential and must not hold up the boot, and the operator who clicked this
 * button should see the result on the card they clicked — not behind a
 * full-screen "Signing in…" that has nothing to do with signing in.
 */
export function ConnectTinyHumansButton({
  client,
  company,
  available,
  canManage,
  configured,
  onConnected,
  hint = true,
}: Props) {
  const [busy, setBusy] = useState(false);
  // The redemption runs from an effect, and StrictMode double-invokes effects.
  // The code is single-use, so a second call would spend nothing and report the
  // host's "expired" refusal over a connection that in fact succeeded.
  const redeeming = useRef(false);

  const finish = useCallback(
    async (state: string, code: string) => {
      setBusy(true);
      try {
        const result = await finishCredentialLink(client, company, state, code);
        toast.success("Connected to TinyHumans.", { description: result.note });
        onConnected?.();
      } catch (err) {
        // The host's own words where it sent them: "that connection attempt has
        // expired" tells an operator to click again, which a generic failure
        // does not.
        toast.error(
          err instanceof ApiError ? err.message : "Couldn't finish connecting to TinyHumans.",
        );
      } finally {
        setBusy(false);
      }
    },
    [client, company, onConnected],
  );

  useEffect(() => {
    if (redeeming.current) return;
    if (takeKeyLinkRefusal()) {
      redeeming.current = true;
      // Cancelling on the hub's consent screen lands here too, which is why this
      // is not worded as an error. Nothing was created either way.
      toast.info("No key was created.", {
        description: "The TinyHumans connection was cancelled or refused.",
      });
      return;
    }
    const pending = takeKeyLink();
    if (!pending) return;
    redeeming.current = true;
    void finish(pending.state, pending.code);
  }, [finish]);

  const start = useCallback(async () => {
    setBusy(true);
    try {
      const { authorizeUrl } = await startCredentialLink(client, company);
      // A top-level navigation, not a fetch: the person signs in on the hub's
      // own origin and approves there, and both need its address bar visible.
      window.location.assign(authorizeUrl);
    } catch (err) {
      toast.error(
        err instanceof ApiError ? err.message : "Couldn't start the TinyHumans connection.",
      );
      setBusy(false);
    }
  }, [client, company]);

  // A host with no hub renders nothing at all, so the page it sits on looks
  // exactly as it did before this flow existed — the paste field, alone.
  if (!available || !canManage) return null;

  return (
    <div className="space-y-2">
      <Button
        type="button"
        disabled={busy}
        onClick={() => void start()}
        data-testid="connect-tinyhumans"
      >
        {busy ? <Loader2 className="size-4 animate-spin" /> : <Sparkles className="size-4" />}
        {configured ? "Reconnect TinyHumans" : "Connect TinyHumans"}
      </Button>
      {hint && (
        <p className="text-xs text-muted-foreground">
          Sign in to TinyHumans and this company gets its key automatically — nothing to copy. It
          covers both the model your agents think with and the accounts they connect.
        </p>
      )}
    </div>
  );
}
