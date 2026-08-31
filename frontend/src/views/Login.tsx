import { useEffect, useRef, useState } from "react";
import {
  ArrowRight,
  Building2,
  Loader2,
  MailCheck,
  Monitor,
  TriangleAlert,
  Wallet,
} from "lucide-react";

import {
  fetchAuthConfig,
  fetchHubProviders,
  loginWithPassword,
  requestCode,
  requestWalletChallenge,
  verifyCode,
  verifyWalletSignature,
  type AuthConfig,
  type HubProvider,
  type SignIn,
} from "@/api/auth";
import { connectWallet, hasWallet, NoWalletError, signMessage } from "@/lib/wallet";
import { resendLabel, secondsUntilResend } from "@/views/login/resend";
import { arrivedViaSetupHandoff, SETUP_HANDOFF_FRAGMENT } from "@/setup/state";
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
  /**
   * Why they landed here, when it was not simply "not signed in yet".
   *
   * Set after a refused ecosystem sign-in *or* a magic link that would not
   * redeem — the expired-or-spent link being by far the commonest of the two,
   * since every email-mode sign-in is one and they last fifteen minutes.
   * Without it a rejected or ineligible sign-in renders an ordinary form and
   * looks like the click did nothing — the one failure mode most likely to be
   * reported as "the button is broken" (issue #1305). It never names an
   * address, so it cannot become the membership oracle the rest of this view
   * refuses to be.
   *
   * A notice is also what puts the caret in the email field below, so that
   * "request a new one" is a keystroke rather than a hunt.
   */
  notice?: string;
  /**
   * Reports a completed sign-in.
   *
   * Handed the whole {@link SignIn}, not just the user, because a cross-origin
   * sign-in returns a session the caller has to store — this view is not where
   * a credential belongs, but it is the only place that sees one arrive.
   */
  onSignedIn: (result: SignIn) => void;
}

type Mode = "link" | "password";

/**
 * What the host says before anything has been fetched.
 *
 * `email` because that is what every company did before the mode was
 * configurable, so an older host — which has no `/auth/config` route — renders
 * exactly the screen it always did. `magicLink` is assumed to work for the same
 * reason: a host that has not told us otherwise is one that either mails links
 * or echoes them, and starting from false would blank the form on every
 * deployment for the length of one fetch.
 */
const ASSUMED_CONFIG: AuthConfig = { mode: "email", passwords: true, magicLink: true };

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
export function Login({ client, company, notice, onSignedIn }: Props) {
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
  /**
   * How this company signs people in.
   *
   * Asked of the host, never inferred from which routes fail: a wallet company
   * and a misconfigured email company both refuse `auth/request`, and only one
   * of them should be offered a wallet button.
   */
  const [authConfig, setAuthConfig] = useState<AuthConfig>(ASSUMED_CONFIG);
  // Always blank. The desktop used to prefill a synthetic
  // `operator@opencompany.local` here, because a packaged install admitted that
  // one address and mailed nothing, so a blank field asked a person to guess a
  // credential they had never seen (#632). The desktop now runs `none` mode and
  // has no address at all, and it was the only caller that ever knew one — so
  // there is nothing left to suggest, and a form on any other host is a form
  // where the person genuinely knows their own address.
  const [email, setEmail] = useState("");
  const [password, setPassword] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [sent, setSent] = useState(false);
  // Only ever set on a host with no mail transport (local dev).
  const [devCode, setDevCode] = useState<string | null>(null);
  /**
   * When the last link was asked for, or `null` if none has been.
   *
   * Stamped from the *response*, not the submit: the host's window opens when
   * it mints the code, which is fractionally earlier, and a clock started late
   * can only ever be conservative. Started early it would let the resend fire
   * into a throttle that answers `202` regardless — a button that reports a
   * send which did not happen, which is worse than no button.
   */
  const [linkSentAt, setLinkSentAt] = useState<number | null>(null);
  /** Ticks the countdown. Only advanced while there is one to render. */
  const [now, setNow] = useState(() => Date.now());
  /** Set when a *re*send lands, so the second press is acknowledged as one. */
  const [resent, setResent] = useState(false);

  useEffect(() => {
    let cancelled = false;
    fetchAuthConfig(client, company)
      .then((config) => {
        if (!cancelled) setAuthConfig(config);
      })
      .catch(() => {
        // `fetchAuthConfig` already falls back to email; this is belt and braces.
      });
    return () => {
      cancelled = true;
    };
  }, [client, company]);

  // The real config can arrive *after* someone has already switched into
  // password mode off the optimistic `ASSUMED_CONFIG`. If it turns out this
  // host does not offer passwords, password mode must not survive that —
  // otherwise the form below stays open on a route that will refuse it.
  useEffect(() => {
    if (!authConfig.passwords && mode === "password") setMode("link");
  }, [authConfig.passwords, mode]);

  /**
   * A host in `email` mode that can deliver nothing.
   *
   * Not the same as "no email sign-in": hub OAuth and passwords never touch a
   * mailbox, so this company still signs people in exactly as it says it does.
   * What is dead is the *link*, and only the host knows that — `auth/request`
   * answers `sent: true` here precisely as it does where the mail went out.
   */
  const linkGoesNowhere = authConfig.mode === "email" && !authConfig.magicLink;

  /**
   * Step out of link mode once, on such a host, when there is somewhere to step.
   *
   * Once, deliberately: someone may switch back on purpose — an operator who
   * has just configured a transport behind this very screen is the likeliest
   * visitor here — and a rule that re-applied itself would make the toggle
   * beneath the form unusable rather than merely mistaken.
   */
  const demotedLink = useRef(false);
  useEffect(() => {
    if (demotedLink.current || !linkGoesNowhere || !authConfig.passwords) return;
    demotedLink.current = true;
    setMode("password");
  }, [linkGoesNowhere, authConfig.passwords]);

  useEffect(() => {
    let cancelled = false;
    // A dead setup hand-off link keeps its marker in the hash while it falls
    // back to this form, so an ecosystem button asked for from here must land on
    // the same destination the link promised — the host carries it on the
    // sign-in's return URI (`from=setup`), which survives the OAuth round trip
    // the way a fragment cannot. Absent for any other sign-in, which lands
    // wherever it always did.
    fetchHubProviders(client, company, arrivedViaSetupHandoff() ? "setup" : undefined)
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

  /**
   * Seconds before the host would mail another link to this address.
   *
   * Derived, never stored: a stored counter and a re-render disagree the moment
   * a tab is backgrounded, and `setInterval` is throttled to a crawl there.
   * Recomputing from two timestamps means a tab woken after ten minutes renders
   * a ready button on its first frame rather than counting the rest of the way
   * down from where it fell asleep.
   */
  const secondsLeft = linkSentAt === null ? 0 : secondsUntilResend(linkSentAt, now);
  const waitingToResend = secondsLeft > 0;

  /** The line under the heading. Empty means the heading stands alone. */
  const headingNote = subtitle(authConfig, mode, sent);

  // Four ticks a second, and only while something is counting. The label is in
  // whole seconds, so a 1s interval would show each number for anywhere between
  // 0 and 1s depending on when the send landed relative to the tick.
  useEffect(() => {
    if (!waitingToResend) return;
    const timer = window.setInterval(() => setNow(Date.now()), 250);
    return () => window.clearInterval(timer);
  }, [waitingToResend]);

  async function sendLink(e: React.FormEvent) {
    e.preventDefault();
    setResent(false);
    await askForLink();
  }

  /**
   * Asks for another link from the sent screen.
   *
   * The whole reason this exists: the sent card is the terminal screen of the
   * primary sign-in path and the one people stare at when nothing arrives, and
   * until #1333 its only control cleared the form. "Retype the address you just
   * typed" is not a recovery path — it gives no sign it is a *re*send, and no
   * sign of the minute the host makes you wait.
   */
  async function resendLink() {
    setResent(false);
    if (await askForLink()) setResent(true);
  }

  /** The one request both paths make. Reports whether the host acknowledged it. */
  async function askForLink(): Promise<boolean> {
    setBusy(true);
    setError(null);
    try {
      const result = await requestCode(
        client,
        company,
        email,
        // A dead setup hand-off link keeps the address while it falls back to
        // this form (`#/company?from=setup` survives in the hash), so a link
        // asked for from here must carry the same destination the original did
        // — otherwise following the replacement lands on Overview and can show
        // the tour welcome instead of the roster setup just built. Absent for
        // any other sign-in, which lands wherever it always did.
        arrivedViaSetupHandoff() ? SETUP_HANDOFF_FRAGMENT : undefined,
      );
      // Always the same acknowledgement, whoever they are.
      setSent(true);
      setDevCode(result.dev_code ?? null);
      setLinkSentAt(Date.now());
      return true;
    } catch (err) {
      setError(friendly(err));
      return false;
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

  /**
   * Connect, sign the host's challenge, and exchange it for a session.
   *
   * The message is signed exactly as the host sent it. Nothing here inspects or
   * rebuilds it: the layout is the host's, versioned by its first line, and a
   * console that assembled its own would silently stop verifying the day the
   * host changed it.
   */
  async function signInWithWallet() {
    setBusy(true);
    setError(null);
    try {
      const address = await connectWallet();
      const challenge = await requestWalletChallenge(client, company, address);
      const signature = await signMessage(challenge.message);
      onSignedIn(await verifyWalletSignature(client, company, challenge.nonce, signature));
    } catch (err) {
      setError(friendly(err));
    } finally {
      setBusy(false);
    }
  }

  const heading = headingFor(authConfig);

  return (
    /*
      The box that owns the viewport height has to be the flex container too.

      `justify-center` distributes free space along its *own* main axis, so on a
      `main` whose parent is a plain block it is inert: that `main` is exactly as
      tall as its content and has no free space to distribute. The height lived
      on this div and the centring lived on `main`, and neither reached the
      other — the card sat pinned under the header with half the viewport empty
      below it (#1332). `flex flex-col` here plus `flex-1` there hands `main`
      the height the header left over, which is what the `justify-center` below
      was always reaching for.

      `flex-1` cannot clip the taller variants: a flex item's `min-height: auto`
      floors it at its content height, so on a short viewport `main` grows past
      its share, the page grows past `min-h-svh`, and the document scrolls.
    */
    <div className="flex min-h-svh flex-col bg-background">
      <header className="flex shrink-0 items-center justify-between border-b px-6 py-4">
        {/* `data-slot` so a deployment can restyle the product mark with CSS
            alone — the same convention `components/ui/*` already uses. Without a
            handle here, every white-label deployment has to patch this JSX and
            re-patch it on every update. */}
        <div data-slot="brand" className="flex items-center gap-2">
          <div
            data-slot="brand-mark"
            className="flex size-7 items-center justify-center rounded-md bg-primary text-primary-foreground"
          >
            <Building2 className="size-4" />
          </div>
          <span data-slot="brand-name" className="text-sm font-semibold">
            OpenCompany
          </span>
        </div>
        <ThemeToggle />
      </header>

      <main className="mx-auto flex w-full max-w-md flex-1 flex-col justify-center px-6 py-16">
        {/*
          The refusal, above the heading, where the eye lands first.

          Given the icon and full-strength text on purpose. `Alert`'s default
          variant is card-coloured with a muted description, which on this page
          sits on a card-coloured form — so an unadorned notice reads as chrome
          and gets skimmed past, which is barely better than the nothing it
          replaced (#1305). Not `destructive`: an expired link is the ordinary
          fate of one left in a mailbox overnight, not an error someone made.
        */}
        {notice && (
          <Alert className="mb-6" data-testid="login-notice">
            <TriangleAlert className="size-4" />
            <AlertDescription className="text-foreground">{notice}</AlertDescription>
          </Alert>
        )}

        {/*
          Rendered only when there is something to say. Both lines are empty in
          `none` mode on a host too old to report a name, and the block used to
          render regardless — an empty `h1` above an empty `p`, which is a
          visible gap, a page with no heading text for a screen reader, and a
          hole in the document outline (issue #1334).
        */}
        {heading || headingNote ? (
          <div className="mb-6 space-y-1">
            {heading ? (
              /*
                Long names wrap rather than truncate — the whole point of
                naming the company here is that someone can confirm it, and
                half a name confirms nothing. `break-words` keeps a name
                with no space in it inside the `max-w-md` column instead of
                pushing the page sideways, and `line-clamp-3` is the
                backstop past which a name is no longer a heading: three lines
                of it, then an ellipsis, with `title` holding the rest.
              */
              <h1
                className="line-clamp-3 text-2xl font-semibold tracking-tight break-words"
                title={heading}
                data-testid="login-heading"
              >
                {heading}
              </h1>
            ) : null}
            {headingNote ? <p className="text-sm text-muted-foreground">{headingNote}</p> : null}
          </div>
        ) : null}

        {/*
          A company with no sign-in. Reaching this screen at all means the
          console could not authenticate — on a `none`-mode host every request
          is already the owner's, so the ordinary path never renders this view.
          What is left is a genuine misconfiguration: something is addressing a
          desktop company from somewhere that is not the desktop. Say that,
          rather than offering a form whose every field is refused.
        */}
        {authConfig.mode === "none" ? (
          <Card className="space-y-3 p-6">
            <div className="flex items-start gap-3">
              <Monitor className="mt-0.5 size-5 shrink-0 text-primary" />
              <div className="space-y-1">
                <p className="text-sm font-medium">There is no sign-in here</p>
                <p className="text-sm text-muted-foreground">
                  This company is used from the OpenCompany app on the computer
                  it runs on. It admits one person — whoever is at that machine —
                  and has no accounts to sign in with.
                </p>
              </div>
            </div>
            {error ? (
              <Alert variant="destructive">
                <AlertDescription>{error}</AlertDescription>
              </Alert>
            ) : null}
          </Card>
        ) : null}

        {/*
          Wallet sign-in. One button, because there is nothing to type: the
          address comes from the wallet, and typing one you do not hold proves
          nothing anyway.
        */}
        {authConfig.mode === "wallet" ? (
          <Card className="space-y-4 p-6">
            {hasWallet() ? (
              <>
                <p className="text-sm text-muted-foreground">
                  Your wallet will be asked to sign a one-time message. It is a
                  signature, not a transaction — nothing is sent and nothing is
                  spent.
                </p>
                <Button className="w-full" onClick={signInWithWallet} disabled={busy}>
                  {busy ? <Loader2 className="mr-2 size-4 animate-spin" /> : null}
                  <Wallet className="mr-2 size-4" />
                  Continue with wallet
                </Button>
              </>
            ) : (
              <div className="flex items-start gap-3">
                <Wallet className="mt-0.5 size-5 shrink-0 text-muted-foreground" />
                <div className="space-y-1">
                  <p className="text-sm font-medium">No wallet found</p>
                  <p className="text-sm text-muted-foreground">
                    This company signs people in with a Solana wallet. Install one
                    in this browser, then reload.
                  </p>
                </div>
              </div>
            )}
            {error ? (
              <Alert variant="destructive">
                <AlertDescription>{error}</AlertDescription>
              </Alert>
            ) : null}
          </Card>
        ) : null}

        {/*
          Ecosystem sign-in, above the form because it is the path most people
          take: one click, no mailbox round trip. Rendered only when the host
          says it has a hub — a self-hosted console shows the form alone.

          Each button is a plain link to a host-supplied URL, not a fetch: the
          hub's OAuth start is a top-level navigation, and the browser must own
          it so the provider's own domain appears in the address bar.
        */}
        {authConfig.mode === "email" && hubProviders.length > 0 && (
          <div className="mb-6 space-y-3">
            <div className="grid gap-2">
              {hubProviders.map((provider) => (
                <a
                  key={provider.id}
                  href={
                    arrivedViaSetupHandoff()
                      ? `${provider.startUrl}${provider.startUrl.includes("?") ? "&" : "?"}from=setup`
                      : provider.startUrl
                  }
                  className={cn(buttonVariants({ variant: "outline", size: "lg" }), "w-full")}
                >
                  Continue with {provider.label}
                </a>
              ))}
            </div>

            <p className="text-center text-2xs leading-5 text-muted-foreground">
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

        {/*
          Said here because it is the last place it can be said. Every other
          surface reports a link as sent, so an operator who is never told will
          type an address, be thanked, and wait for a message no process on this
          host will ever produce. The form stays below regardless: mail can be
          configured without restarting this console, and a person who knows a
          link is coming should still be able to ask for one.
        */}
        {linkGoesNowhere && (
          <Alert className="mb-4" data-testid="login-no-mail">
            <TriangleAlert className="size-4" />
            <AlertDescription className="text-foreground">
              {hubProviders.length > 0
                ? `This host can't send mail, so a sign-in link won't arrive. Use one of the buttons above${authConfig.passwords ? ", or the password you set for this company." : "."}`
                : authConfig.passwords
                  ? "This host can't send mail, so a sign-in link won't arrive. Sign in with the password you set for this company — an admin can issue you one if you have none."
                  : "This host can't send mail, so a sign-in link won't arrive. Whoever runs it needs to configure a mail transport before this screen can sign anyone in."}
            </AlertDescription>
          </Alert>
        )}

        {authConfig.mode === "email" ? (
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

              {/*
                The error alert has to be repeated here rather than lifted out
                of the form below: until #1333 the only request this screen
                could make was made from the form, so an alert rendered only
                inside the form was sufficient. A resend that fails on a screen
                with no error slot would fail invisibly — the counter would
                restart and nothing else would change.
              */}
              {error ? (
                <Alert variant="destructive">
                  <AlertDescription>{error}</AlertDescription>
                </Alert>
              ) : null}

              {resent && !error ? (
                <p className="text-sm text-muted-foreground" data-testid="login-resent">
                  Sent again. The newest link is the one that works.
                </p>
              ) : null}

              <div className="flex flex-wrap items-center gap-2">
                {/*
                  The card's own action, and the strongest thing on it: this is
                  the screen someone is looking at *because* the mail has not
                  arrived. Disabled for the host's minute rather than hidden, so
                  the wait is a visible fact rather than a missing control.
                */}
                <Button
                  variant="outline"
                  size="sm"
                  onClick={resendLink}
                  disabled={busy || waitingToResend}
                  data-testid="login-resend"
                >
                  {busy ? <Loader2 className="size-4 animate-spin" /> : null}
                  {resendLabel(secondsLeft)}
                </Button>

                {/* Demoted below the resend, but still underlined-on-hover
                    primary text rather than the unadorned `ghost` label it was
                    — which, as the only control in the state, did not read as
                    a control at all. */}
                <Button
                  variant="link"
                  size="sm"
                  onClick={() => {
                    setSent(false);
                    setDevCode(null);
                    setResent(false);
                    setError(null);
                  }}
                >
                  Use a different address
                </Button>
              </div>
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
                  // Only when something was refused. Whoever is reading a
                  // notice has one thing left to do — ask for another link —
                  // and this makes it a keystroke instead of a hunt for the
                  // field. A cold visit is left alone: stealing focus on an
                  // ordinary page load moves the viewport for anyone using a
                  // screen reader or a zoomed browser, for no reason.
                  autoFocus={Boolean(notice)}
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
        ) : null}

        {/*
          Hidden while the link is out. Offering a different credential type as
          a peer of "go look in your mailbox" muddles a screen whose whole job
          at that moment is the mailbox — and pressing it silently threw the
          link away, since the handler clears `sent` with no acknowledgement
          (issue #1333). "Use a different address" is the way back to the form,
          and this returns with it.
        */}
        {authConfig.mode === "email" && authConfig.passwords && !(sent && mode === "link") ? (
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
        ) : null}

        {authConfig.mode === "email" && mode === "password" ? (
          <p className="mt-2 text-center text-xs text-muted-foreground">
            Forgot it? Sign in with a link, then set a new password.
          </p>
        ) : null}
      </main>
    </div>
  );
}

/**
 * What this screen is a sign-in *to*.
 *
 * The name comes from `/auth/config` — the one thing the host tells the console
 * before anybody has a credential — so on a host too old to report it this is
 * the bare "Sign in" every deployment showed before, not a blank.
 *
 * `none` mode gets the name alone: there is no signing in to do there, the card
 * below says so in full, and "Sign in to Acme" over "There is no sign-in here"
 * would contradict itself. With no name it gets nothing, and the block that
 * would have held it is not rendered at all.
 */
function headingFor(config: AuthConfig): string {
  if (config.mode === "none") return config.name ?? "";
  return config.name ? `Sign in to ${config.name}` : "Sign in";
}

/** One line under the heading, saying what this company will actually ask for. */
function subtitle(config: AuthConfig, mode: Mode, sent: boolean): string {
  if (config.mode === "none") return "";
  if (config.mode === "wallet") return "Prove you hold the wallet. Nothing is emailed.";
  // Nothing, once the link is gone. Every remaining line here is written in the
  // future tense about a form that is no longer on screen, and "We'll email you
  // a link" sitting 20px above "Check your email" is two tenses making two
  // different claims about the same act (issue #1333). The card carries the
  // whole message by then, so there is nothing left to add.
  if (sent && mode === "link") return "";
  // Promising a link from a host with no transport is the one line here that
  // sends someone away to wait for nothing.
  if (mode === "link" && !config.magicLink) return "This host can\'t email you a link.";
  return mode === "link"
    ? "We\'ll email you a link. No password needed."
    : "Use the password you set for this company.";
}

/**
 * Renders an error without inventing detail the API withheld.
 *
 * `invalid_login` is the backend's single, deliberate answer for every failure —
 * unknown address, wrong password, expired link, spent link. It stays vague
 * here for the same reason it is vague there.
 */
function friendly(err: unknown): string {
  if (err instanceof NoWalletError) {
    return "No wallet found in this browser. Install one, then reload.";
  }
  if (err instanceof ApiError) {
    if (err.code === "auth_mode") {
      // The console asked for a sign-in this company does not have. Almost
      // always a stale tab against a host whose mode changed under it.
      return `${err.message}. Reload to see the right sign-in.`;
    }
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
