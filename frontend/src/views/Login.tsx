import { useEffect, useState } from "react";
import { ArrowRight, Building2, Loader2, MailCheck } from "lucide-react";

import {
  fetchHubProviders,
  loginWithPassword,
  requestCode,
  verifyCode,
  type HubProvider,
  type Me,
} from "@/api/auth";
import type { OpenCompanyClient } from "@/api/client";
import { ApiError } from "@/api/types";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Button, buttonVariants } from "@/components/ui/button";
import { Card } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { ThemeToggle } from "@/components/theme-toggle";
import { cn } from "@/lib/utils";

/** Where the ecosystem's terms live. Same documents OpenHuman links from its welcome screen. */
const TERMS_OF_USE_URL = "https://tinyhumans.gitbook.io/openhuman/legal/terms-of-use";
const PRIVACY_POLICY_URL = "https://tinyhumans.gitbook.io/openhuman/legal/privacy-policy";

interface Props {
  client: OpenCompanyClient;
  company: string | null;
  /** The company's display name, when the host would tell us before sign-in. */
  companyName?: string;
  /**
   * Why they landed here, when it was not simply "not signed in yet".
   *
   * Set after a refused ecosystem sign-in. Without it a rejected or ineligible
   * sign-in renders an ordinary form and looks like the click did nothing — the
   * one failure mode most likely to be reported as "the button is broken". It
   * never names an address, so it cannot become the membership oracle the rest
   * of this view refuses to be.
   */
  notice?: string;
  /** A code lifted out of a magic-link URL, redeemed on mount by the caller. */
  onSignedIn: (me: Me) => void;
}

type Mode = "link" | "password";

/**
 * The sign-in view: magic link by default, password for anyone who set one.
 *
 * Two rules this view must not break:
 *
 * 1. **Never say whether an account exists.** The backend answers identically
 *    for a member and a stranger, deliberately, so that nobody can enumerate a
 *    company's membership. Rendering "no such user" here would hand back the
 *    oracle the API refuses to be.
 * 2. **Never store the session.** It arrives as an HttpOnly cookie the browser
 *    keeps; there is nothing to put in localStorage and nothing for an XSS to
 *    steal.
 */
export function Login({ client, company, companyName, notice, onSignedIn }: Props) {
  const [mode, setMode] = useState<Mode>("link");
  /**
   * The ecosystem sign-in buttons this host offers, or `[]` if it offers none.
   *
   * Asked of the host rather than assumed, because only the host knows the
   * hub's base URL and the origin the hub must return to. A self-hosted host
   * answers with an empty list and this view renders the magic-link form alone,
   * which is why a failure to fetch is swallowed: no buttons is a valid state,
   * not an error worth showing anyone.
   */
  const [hubProviders, setHubProviders] = useState<HubProvider[]>([]);
  const [email, setEmail] = useState("");
  const [password, setPassword] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [sent, setSent] = useState(false);
  // Only ever set on a host with no mail transport (local dev).
  const [devCode, setDevCode] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    fetchHubProviders(client, company)
      .then((providers) => {
        if (!cancelled) setHubProviders(providers);
      })
      .catch(() => {
        // No buttons. The form below still works, and a host that cannot answer
        // this question could not have completed the flow anyway.
      });
    return () => {
      cancelled = true;
    };
  }, [client, company]);

  async function sendLink(e: React.FormEvent) {
    e.preventDefault();
    setBusy(true);
    setError(null);
    try {
      const result = await requestCode(client, company, email);
      // Always the same acknowledgement, whoever they are.
      setSent(true);
      setDevCode(result.dev_code ?? null);
    } catch (err) {
      setError(friendly(err));
    } finally {
      setBusy(false);
    }
  }

  async function signInWithPassword(e: React.FormEvent) {
    e.preventDefault();
    setBusy(true);
    setError(null);
    try {
      onSignedIn(await loginWithPassword(client, company, email, password));
    } catch (err) {
      setError(friendly(err));
    } finally {
      setBusy(false);
    }
  }

  async function redeemDevCode() {
    if (!devCode) return;
    setBusy(true);
    setError(null);
    try {
      onSignedIn(await verifyCode(client, company, devCode));
    } catch (err) {
      setError(friendly(err));
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className="min-h-svh bg-background">
      <header className="flex items-center justify-between border-b px-6 py-4">
        <div className="flex items-center gap-2">
          <div className="flex size-7 items-center justify-center rounded-md bg-primary text-primary-foreground">
            <Building2 className="size-4" />
          </div>
          <span className="text-sm font-semibold">OpenCompany</span>
        </div>
        <ThemeToggle />
      </header>

      <main className="mx-auto flex w-full max-w-md flex-col justify-center px-6 py-16">
        {notice && (
          <Alert className="mb-6">
            <AlertDescription>{notice}</AlertDescription>
          </Alert>
        )}

        <div className="mb-6 space-y-1">
          <h1 className="text-2xl font-semibold tracking-tight">
            Sign in{companyName ? ` to ${companyName}` : ""}
          </h1>
          <p className="text-sm text-muted-foreground">
            {mode === "link"
              ? "We'll email you a link. No password needed."
              : "Use the password you set for this company."}
          </p>
        </div>

        {/*
          Ecosystem sign-in, above the form because it is the path most people
          take: one click, no mailbox round trip. Rendered only when the host
          says it has a hub — a self-hosted console shows the form alone.

          Each button is a plain link to a host-supplied URL, not a fetch: the
          hub's OAuth start is a top-level navigation, and the browser must own
          it so the provider's own domain appears in the address bar.
        */}
        {hubProviders.length > 0 && (
          <div className="mb-6 space-y-3">
            <div className="grid gap-2">
              {hubProviders.map((provider) => (
                <a
                  key={provider.id}
                  href={provider.startUrl}
                  className={cn(buttonVariants({ variant: "outline", size: "lg" }), "w-full")}
                >
                  Continue with {provider.label}
                </a>
              ))}
            </div>

            <p className="text-center text-[11px] leading-5 text-muted-foreground">
              By continuing, you agree to the{" "}
              <a
                href={TERMS_OF_USE_URL}
                target="_blank"
                rel="noreferrer"
                className="font-medium underline underline-offset-2 hover:text-foreground"
              >
                Terms
              </a>{" "}
              and{" "}
              <a
                href={PRIVACY_POLICY_URL}
                target="_blank"
                rel="noreferrer"
                className="font-medium underline underline-offset-2 hover:text-foreground"
              >
                Privacy Policy
              </a>
              .
            </p>

            <div className="flex items-center gap-3">
              <div className="h-px flex-1 bg-border" />
              <span className="text-xs text-muted-foreground">or</span>
              <div className="h-px flex-1 bg-border" />
            </div>
          </div>
        )}

        <Card className="p-6">
          {sent && mode === "link" ? (
            <div className="space-y-4">
              <div className="flex items-start gap-3">
                <MailCheck className="mt-0.5 size-5 shrink-0 text-primary" />
                <div className="space-y-1">
                  <p className="text-sm font-medium">Check your email</p>
                  <p className="text-sm text-muted-foreground">
                    If {email} can sign in here, a link is on its way. It expires in
                    15 minutes and works once.
                  </p>
                </div>
              </div>

              {devCode ? (
                <Alert>
                  <AlertDescription className="space-y-2">
                    <p className="text-xs">
                      This host has no email configured, so the link was returned
                      instead of sent. That only happens in local development.
                    </p>
                    <Button size="sm" onClick={redeemDevCode} disabled={busy}>
                      {busy ? <Loader2 className="size-4 animate-spin" /> : null}
                      Use it now
                    </Button>
                  </AlertDescription>
                </Alert>
              ) : null}

              <Button
                variant="ghost"
                size="sm"
                onClick={() => {
                  setSent(false);
                  setDevCode(null);
                }}
              >
                Use a different address
              </Button>
            </div>
          ) : (
            <form
              className="space-y-4"
              onSubmit={mode === "link" ? sendLink : signInWithPassword}
            >
              <div className="space-y-2">
                <Label htmlFor="email">Email</Label>
                <Input
                  id="email"
                  type="email"
                  autoComplete="username"
                  required
                  value={email}
                  onChange={(e) => setEmail(e.target.value)}
                  placeholder="you@company.com"
                />
              </div>

              {mode === "password" ? (
                <div className="space-y-2">
                  <Label htmlFor="password">Password</Label>
                  <Input
                    id="password"
                    type="password"
                    autoComplete="current-password"
                    required
                    value={password}
                    onChange={(e) => setPassword(e.target.value)}
                  />
                </div>
              ) : null}

              {error ? (
                <Alert variant="destructive">
                  <AlertDescription>{error}</AlertDescription>
                </Alert>
              ) : null}

              <Button type="submit" className="w-full" disabled={busy}>
                {busy ? <Loader2 className="mr-2 size-4 animate-spin" /> : null}
                {mode === "link" ? "Email me a link" : "Sign in"}
                {!busy ? <ArrowRight className="ml-2 size-4" /> : null}
              </Button>
            </form>
          )}
        </Card>

        <div className="mt-4 text-center">
          <Button
            variant="link"
            size="sm"
            onClick={() => {
              setMode(mode === "link" ? "password" : "link");
              setError(null);
              setSent(false);
            }}
          >
            {mode === "link" ? "Use a password instead" : "Email me a link instead"}
          </Button>
        </div>

        {mode === "password" ? (
          <p className="mt-2 text-center text-xs text-muted-foreground">
            Forgot it? Sign in with a link, then set a new password.
          </p>
        ) : null}
      </main>
    </div>
  );
}

/**
 * Renders an error without inventing detail the API withheld.
 *
 * `invalid_login` is the backend's single, deliberate answer for every failure —
 * unknown address, wrong password, expired link, spent link. It stays vague
 * here for the same reason it is vague there.
 */
function friendly(err: unknown): string {
  if (err instanceof ApiError) {
    if (err.code === "invalid_login") {
      return "That didn't work. Check the address and password, or sign in with a link.";
    }
    if (err.status === 0) {
      return "Can't reach the company host.";
    }
    return err.message;
  }
  return "Something went wrong. Try again.";
}
