import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { Loader2 } from "lucide-react";

import { signInWithHubToken, verifyCode } from "@/api/auth";
import { OpenCompanyClient } from "@/api/client";
import { ApiError, type CompanyStatus } from "@/api/types";
import { AppShell } from "@/components/app-shell";
import { CompanyPicker } from "@/components/company-picker";
import { Login } from "@/views/Login";
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import { resolveConfig } from "@/config";

type Phase =
  | { kind: "loading" }
  | { kind: "error"; message: string; hint?: string }
  | { kind: "login"; company: string | null; notice?: string }
  | { kind: "picker"; companies: CompanyStatus[] }
  | {
      kind: "console";
      company: string | null;
      status: CompanyStatus;
      companies: CompanyStatus[];
      canGoBack: boolean;
    };

/**
 * Reads `?company=&code=` off a magic-link landing.
 *
 * **Pure.** It must stay that way: this runs in a `useMemo`, and StrictMode
 * double-invokes those. Stripping the URL here — as this once did — meant the
 * second invocation read an already-cleaned URL and returned nothing, silently
 * dropping the code and the company. Clearing is a side effect, so it lives in
 * an effect: see `clearMagicLinkFromUrl`.
 */
function readMagicLink(): { company: string | null; code: string } | null {
  const params = new URLSearchParams(window.location.search);
  const code = params.get("code");
  if (!code) return null;
  return { company: params.get("company"), code };
}

/**
 * Reads `?token=&key=auth` off a hub sign-in landing.
 *
 * The hub appends these to the redirect URI it was given, so they arrive on a
 * plain top-level navigation back to this console. `key=auth` is the hub's own
 * marker for that redirect and is what distinguishes this token from the
 * `?token=` the console config uses for a platform bearer — see `config.ts`.
 *
 * **Pure**, for the same reason `readMagicLink` is: StrictMode double-invokes
 * the `useMemo` this runs in, so stripping the URL here would make the second
 * invocation read a cleaned URL and silently drop the token.
 *
 * A failed sign-in comes back as `?error=` instead, which is not read here —
 * the hub's error text is its own wording about its own flow, and this console
 * says its piece in `hubNotice`.
 */
function readHubToken(): string | null {
  const params = new URLSearchParams(window.location.search);
  if (params.get("key") !== "auth") return null;
  return params.get("token");
}

/** Whether the hub bounced the sign-in back with a failure rather than a token. */
function readHubError(): boolean {
  const params = new URLSearchParams(window.location.search);
  return params.get("key") === "auth" && params.get("error") !== null;
}

/**
 * Strips the magic link out of the address bar.
 *
 * The code is a single-use credential, so it must not linger in the URL, the
 * history, or a `Referer` header once we hold it.
 */
function clearMagicLinkFromUrl(): void {
  const params = new URLSearchParams(window.location.search);
  if (!params.has("code")) return;
  params.delete("code");
  params.delete("company");
  const query = params.toString();
  window.history.replaceState({}, "", window.location.pathname + (query ? `?${query}` : ""));
}

/**
 * Strips the hub's sign-in result out of the address bar.
 *
 * `replaceState` rather than a push, so the token is gone from the history
 * entry as well as from the bar — a back button that restored it would hand a
 * live ecosystem credential to a reload, and a `Referer` carrying it would hand
 * it to whatever the console links out to next.
 *
 * `company` is deliberately kept. It is not a credential, and dropping it would
 * un-scope the console on a reload of a multi-company host.
 */
function clearHubResultFromUrl(): void {
  const params = new URLSearchParams(window.location.search);
  if (params.get("key") !== "auth") return;
  params.delete("token");
  params.delete("error");
  params.delete("key");
  const query = params.toString();
  window.history.replaceState({}, "", window.location.pathname + (query ? `?${query}` : ""));
}

export function App() {
  const config = useMemo(() => resolveConfig(), []);
  const client = useMemo(() => new OpenCompanyClient(config), [config]);
  const [phase, setPhase] = useState<Phase>({ kind: "loading" });
  // A pure read, so StrictMode's double render is harmless.
  const magicLink = useMemo(() => readMagicLink(), []);
  const hubToken = useMemo(() => readHubToken(), []);
  const hubFailed = useMemo(() => readHubError(), []);
  /**
   * The in-flight redemption, so a link is redeemed exactly once.
   *
   * StrictMode double-invokes effects, and a login code is single-use: the
   * second call would spend nothing and 401, bouncing a perfectly good sign-in
   * to the login screen. Both runs await this one promise instead. A ref rather
   * than a "done" flag, because the second run must *wait for* the first — not
   * skip ahead and query with a session that does not exist yet.
   */
  const redemption = useRef<Promise<unknown> | null>(null);

  // Now that the credential is captured in state, take it out of the URL.
  useEffect(() => {
    if (magicLink) clearMagicLinkFromUrl();
    if (hubToken || hubFailed) clearHubResultFromUrl();
  }, [magicLink, hubToken, hubFailed]);

  // An expired or revoked session anywhere in the console drops to sign-in
  // rather than showing a broken page.
  useEffect(() => {
    client.onUnauthorized = () => setPhase({ kind: "login", company: config.company });
    return () => {
      client.onUnauthorized = null;
    };
  }, [client, config.company]);

  useEffect(() => {
    let cancelled = false;
    const set = (p: Phase) => !cancelled && setPhase(p);

    async function boot() {
      // The hub bounced them back without a token (they cancelled at the
      // provider, or the hub refused the redirect). Nothing to exchange.
      if (hubFailed) {
        set({
          kind: "login",
          company: config.company,
          notice: "That sign-in didn't complete. Try again, or use a link below.",
        });
        return;
      }

      // A hub landing: exchange the platform token for a session here before
      // anything else, for the same reason a magic link does — the session has
      // to exist before the console asks for data.
      if (hubToken) {
        try {
          redemption.current ??= signInWithHubToken(client, config.company, hubToken);
          await redemption.current;
        } catch (err) {
          // A refused sign-in falls back to this company's own form rather than
          // stranding them: the magic link still works, and it is the only
          // thing that works on a self-hosted host anyway.
          set({ kind: "login", company: config.company, notice: hubNotice(err) });
          return;
        }
      }

      // A magic-link landing: redeem it before anything else, so the session
      // exists by the time the console asks for data.
      if (magicLink) {
        const company = magicLink.company ?? config.company;
        try {
          redemption.current ??= verifyCode(client, company, magicLink.code);
          await redemption.current;
        } catch {
          // A dead link is not fatal — fall through to sign-in and let them
          // ask for another. The reason stays vague on purpose.
          set({ kind: "login", company });
          return;
        }
      }

      // Explicit company wins: go straight to its console.
      if (config.company) {
        try {
          const status = await client.status(config.company);
          set({ kind: "console", company: config.company, status, companies: [status], canGoBack: false });
        } catch (err) {
          set(connectionError(client, err, config.company));
        }
        return;
      }

      // Otherwise discover companies from the host.
      try {
        const companies = await client.listCompanies();
        if (companies.length === 1) {
          const c = companies[0];
          set({ kind: "console", company: c.id, status: c, companies, canGoBack: false });
        } else if (companies.length > 1) {
          set({ kind: "picker", companies });
        } else {
          set({
            kind: "error",
            message: "No companies are running on this host.",
            hint: "Start one with `opencompany serve --company <dir>`.",
          });
        }
      } catch (listErr) {
        // Fall back to the single-company alias (prosumer serve).
        try {
          const status = await client.status(null);
          set({ kind: "console", company: null, status, companies: [], canGoBack: false });
        } catch {
          set(connectionError(client, listErr, config.company));
        }
      }
    }

    void boot();
    return () => {
      cancelled = true;
    };
  }, [client, config.company, magicLink, hubToken, hubFailed]);

  const switchCompany = useCallback(
    async (id: string, companies: CompanyStatus[]) => {
      try {
        const status = await client.status(id);
        setPhase({ kind: "console", company: id, status, companies, canGoBack: true });
      } catch (err) {
        setPhase(connectionError(client, err, id));
      }
    },
    [client],
  );

  const backToPicker = useCallback(() => {
    void client.listCompanies().then((companies) => setPhase({ kind: "picker", companies }));
  }, [client]);

  switch (phase.kind) {
    case "loading":
      return (
        <FullScreen>
          <div className="flex items-center gap-2 text-sm text-muted-foreground">
            <Loader2 className="size-4 animate-spin" /> Connecting…
          </div>
        </FullScreen>
      );

    case "login":
      return (
        <Login
          client={client}
          company={phase.company}
          notice={phase.notice}
          onSignedIn={() => window.location.reload()}
        />
      );

    case "error":
      return (
        <FullScreen>
          <div className="w-full max-w-md space-y-4">
            <Alert variant="destructive">
              <AlertTitle>Can&apos;t connect</AlertTitle>
              <AlertDescription>
                {phase.message}
                {phase.hint && <span className="mt-1 block font-mono text-xs opacity-80">{phase.hint}</span>}
              </AlertDescription>
            </Alert>
            <Button className="w-full" onClick={() => location.reload()}>
              Retry
            </Button>
          </div>
        </FullScreen>
      );

    case "picker":
      return (
        <CompanyPicker
          companies={phase.companies}
          onPick={(id) => void switchCompany(id, phase.companies)}
        />
      );

    case "console":
      return (
        <AppShell
          key={phase.company ?? "single"}
          client={client}
          company={phase.company}
          initialStatus={phase.status}
          companies={phase.companies}
          onSwitchCompany={(id) => void switchCompany(id, phase.companies)}
          onBackToPicker={phase.canGoBack ? backToPicker : undefined}
        />
      );
  }
}

function FullScreen({ children }: { children: React.ReactNode }) {
  return (
    <div className="grid min-h-svh place-items-center bg-background p-6 text-center">{children}</div>
  );
}

/**
 * What to tell someone whose ecosystem sign-in did not work.
 *
 * Each line is about the *credential* or the *host*, never about the person:
 * "expired", "no access yet", "not connected". None of them confirms or denies
 * that any address has an account here, which is the rule the whole sign-in
 * surface is built around.
 */
function hubNotice(err: unknown): string {
  const code = err instanceof ApiError ? err.code : "";
  switch (code) {
    case "hub_rejected":
      return "That sign-in expired. Try again, or use a link below.";
    case "not_a_member":
      return "You're signed in to TinyHumans, but this company hasn't given you access yet. Ask an admin to invite you.";
    case "hub_unavailable":
      return "This host isn't connected to a TinyHumans account. Sign in with a link instead.";
    default:
      return "We couldn't complete that sign-in. Try a link below.";
  }
}

function connectionError(client: OpenCompanyClient, err: unknown, company: string | null): Phase {
  const where = client.baseUrl || "this origin";
  if (err instanceof ApiError && err.status === 401) {
    // A 401 now usually means "no session", not "no operator token" — humans
    // sign in. Offering the login view is right for a user and harmless for an
    // operator, who can still pass ?token=. Returning the error phase here
    // would also race the client's onUnauthorized hook and win, stranding a
    // signed-out user on a dead end.
    return { kind: "login", company };
  }
  return {
    kind: "error",
    message: `Couldn't reach a company host at ${where}.`,
    hint: "Set the host with ?api=<url>, or run `opencompany serve`.",
  };
}
