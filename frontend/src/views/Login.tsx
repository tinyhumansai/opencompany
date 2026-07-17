import { useState } from "react";
import { ArrowRight, Building2, Loader2, MailCheck } from "lucide-react";

import { loginWithPassword, requestCode, verifyCode, type Me } from "@/api/auth";
import type { OpenCompanyClient } from "@/api/client";
import { ApiError } from "@/api/types";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import { Card } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { ThemeToggle } from "@/components/theme-toggle";

interface Props {
  client: OpenCompanyClient;
  company: string | null;
  /** The company's display name, when the host would tell us before sign-in. */
  companyName?: string;
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
export function Login({ client, company, companyName, onSignedIn }: Props) {
  const [mode, setMode] = useState<Mode>("link");
  const [email, setEmail] = useState("");
  const [password, setPassword] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [sent, setSent] = useState(false);
  // Only ever set on a host with no mail transport (local dev).
  const [devCode, setDevCode] = useState<string | null>(null);

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
