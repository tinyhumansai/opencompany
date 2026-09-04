// One connection's console: discovery, company selection, and the app shell.
//
// This is the phase machine `App` used to own. Moving it here is what makes a
// second host an *instance* rather than a second code path — and it is what
// makes failure local. `App`'s version set a single global phase, so any host
// being unreachable blanked the whole console and any expired session dropped
// the whole app to a login screen. With N connections that is the wrong shape:
// one host being down has to redden one row and leave the others working.

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { Loader2 } from "lucide-react";

import { OpenCompanyClient } from "@/api/client";
import type { SignIn } from "@/api/auth";
import { ApiError, type CompanyStatus } from "@/api/types";
import { AppShell } from "@/components/app-shell";
import { CompanyPicker } from "@/components/company-picker";
import {
  CREATE_UNAVAILABLE_NOTE,
  CreateCompanyDialog,
  type CreateCompanyRequest,
  offersCompanyCreation,
} from "@/components/create-company-dialog";
import { ConsoleChrome } from "@/components/host-switcher";
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import { ConnectionScopeProvider } from "@/connections/ConnectionContext";
import { useHosts } from "@/connections/HostsContext";
import {
  adoptSession,
  clearDefaultCompany,
  probe,
  retargetCompanyUrlParam,
  retargetDefaultCompany,
  useConnection,
} from "@/connections/registry";
import type { ConnectionId } from "@/connections/types";
import { withHostParam } from "@/hooks/use-host-route";
import { Login } from "@/views/Login";
import { SetupWizard } from "@/views/setup/SetupWizard";

type Phase =
  | { kind: "loading" }
  | { kind: "error"; message: string; hint?: string }
  | { kind: "login"; company: string | null; notice?: string }
  // A host that has never been through the first-run setup flow. Entered ahead
  // of `login` on purpose: an unconfigured host may have no company and no
  // users at all, so a sign-in form there addresses nobody.
  | { kind: "setup" }
  // A configured host (setup has completed) that nonetheless has no
  // companies — an operator who ran setup for host settings alone, or who
  // deleted the only company afterward. Distinct from `error`: this is not a
  // connection problem, and the way out is back into setup, not a retry.
  | { kind: "no-company" }
  | { kind: "picker"; companies: CompanyStatus[] }
  | {
      kind: "console";
      company: string | null;
      status: CompanyStatus;
      companies: CompanyStatus[];
      canGoBack: boolean;
    };

interface Props {
  connectionId: ConnectionId;
  client: OpenCompanyClient;
  /** The company this console was configured to open, if any. */
  defaultCompany: string | null;
  /** A notice from a sign-in attempt that happened before this mounted. */
  notice?: string;
  /** Forces the sign-in view — a magic link that failed to redeem, say. */
  forceLogin?: boolean;
  /**
   * Whether THIS connection is the one `resolveConfig()` produced for the
   * page currently loaded — `App`'s `bootstrapId === connectionId` (issue
   * #1828 comment 3865563560).
   *
   * The only connection whose `?company=`/`?api=` URL params describe it. A
   * restored, non-bootstrap profile (added in a previous session, or
   * selected from the switcher) can carry its own `defaultCompany` just the
   * same, but that value came from `profileStore`, not from the page's URL —
   * so a create/reset on it must retarget the *profile* only. Rewriting the
   * live URL for it clobbers whatever host/company the address bar actually
   * names (the bootstrap connection's, or none at all), and the very next
   * reload's `resolveConfig()`/`findProfile` pair mints a duplicate
   * connection scoped to the wrong host. Defaults to `false` (the safer
   * read) for callers that have not threaded it through yet.
   */
  isBootstrap?: boolean;
}

export function ConnectionConsole({
  connectionId,
  client,
  defaultCompany,
  notice,
  forceLogin,
  isBootstrap = false,
}: Props) {
  const [phase, setPhase] = useState<Phase>(
    forceLogin ? { kind: "login", company: defaultCompany, notice } : { kind: "loading" },
  );
  // Incremented to re-run discovery on demand. The boot effect's dependencies
  // (`client`, `defaultCompany`, `forceLogin`) never change when a sign-in
  // completes, so without an explicit epoch a successful re-login would either
  // hang on "Connecting…" (nothing re-ran the boot) or reload the document
  // (every other connection's stream died with it).
  const [bootEpoch, setBootEpoch] = useState(0);
  const connection = useConnection(connectionId);
  // A `CompanyStatus` this component already resolved for a company id,
  // carried across the reseat-driven reboot below rather than re-fetched.
  //
  // `onCompanyCreated` calls `retargetDefaultCompany` on an explicit-company
  // connection, which `reseat`s it — a brand new client and a new
  // `defaultCompany` — and `App` passes both straight through as this
  // component's props. That prop change re-enters the boot effect, and since
  // `defaultCompany` is now set it takes the "explicit company wins" branch
  // straight into a *second* `client.status(id)` call for the company
  // `switchCompany` already entered with a known-good status a moment
  // earlier (`onCompanyCreated`'s own `knownStatus` argument, PR comment
  // 3864628314 — that fix stopped `switchCompany` from re-fetching; this
  // stops the reboot the reseat itself causes from doing the same fetch a
  // second time, independently). Left unguarded, a transient failure on this
  // reboot lookup — nothing needed it — replaced a fully succeeded
  // create/reset with the generic connection-error screen (issue #1828
  // comment 3865401542).
  const knownStatusRef = useRef<{ company: string; status: CompanyStatus } | null>(null);
  // Read unconditionally, though only the `error` phase uses it: a hook cannot
  // hide inside a switch arm. What it decides is what a failure is allowed to
  // say — see the `error` case below.
  const { connections, onRemoveHost } = useHosts();

  // The registry marks this connection `unauthenticated` when its client sees a
  // 401 — that is the *only* 401 handler, so one host refusing a credential
  // cannot reach into another host's view. Deriving the sign-in screen from
  // that status rather than from a second callback is what keeps it that way.
  const refused = connection?.status === "unauthenticated";

  // Re-probe this connection and re-run the boot effect. A reload would tear
  // down every *other* connection's stream to recover this one; bumping the
  // epoch keeps the recovery local.
  const reBoot = useCallback(
    (result?: SignIn) => {
      // Store the session *before* probing. A cross-origin sign-in's token is
      // the only proof this connection has — no cookie was set — and adoption
      // replaces the credential (in the client here, in the core on the
      // desktop), so a probe that ran first would authenticate with the
      // pre-sign-in credential and conclude the host still refuses us. Hence
      // awaited, not fired: on the desktop the adoption is an IPC round trip,
      // and "before" has to mean before.
      void (async () => {
        if (result?.session) {
          try {
            await adoptSession(connectionId, result.session);
          } catch (error) {
            // The session could not be kept — a locked keychain, a plain-HTTP
            // host the core refuses to carry a credential to. Probing anyway
            // would authenticate as nobody, 401, and land back on this screen
            // wearing the generic "credential refused" face: a person who just
            // signed in successfully, told their credential was wrong. So the
            // sign-in view returns instead, carrying the refusal's own words —
            // the core writes them for exactly this reading ("this host is not
            // encrypted…" names an action; "sign-in failed" names nothing).
            const reason =
              error instanceof Error ? error.message : String(error ?? "the session could not be stored");
            setPhase({
              kind: "login",
              company: defaultCompany,
              notice: `You signed in, but this session could not be kept: ${reason}`,
            });
            return;
          }
        }
        await probe(connectionId);
        setPhase({ kind: "loading" });
        setBootEpoch((n) => n + 1);
      })();
    },
    [connectionId, defaultCompany],
  );

  useEffect(() => {
    // `forceLogin` forces the sign-in view until the *first* boot; a sign-in
    // bumps the epoch, so the forced-login recover cannot re-enter the boot.
    if (forceLogin && bootEpoch === 0) return;
    let cancelled = false;
    const set = (p: Phase) => !cancelled && setPhase(p);

    async function boot() {
      // Ask the host whether it has ever been configured. `/spec` is the
      // unauthenticated handshake, which is what makes this answerable before
      // sign-in — and it has to be, because an unconfigured host has no users
      // to sign in as. A host too old to carry the field omits it, and
      // `!== false` leaves those on the existing path unchanged.
      try {
        const spec = await client.spec();
        if (spec.setup_complete === false) {
          set({ kind: "setup" });
          return;
        }
      } catch {
        // A host that cannot answer `/spec` is a connection problem, not an
        // unconfigured one. Fall through to discovery, which reports it
        // properly.
      }

      // Explicit company wins: go straight to its console.
      if (defaultCompany) {
        // This rerun may exist only because `reseat` changed `client`/
        // `defaultCompany` identity out from under an already-correct
        // console, not because anything needs rebooting — see the
        // `knownStatusRef` comment above. Consume it once and skip the
        // fetch entirely; the phase `switchCompany` already set stands.
        const known = knownStatusRef.current;
        if (known && known.company === defaultCompany) {
          knownStatusRef.current = null;
          return;
        }
        try {
          const status = await client.status(defaultCompany);
          set({
            kind: "console",
            company: defaultCompany,
            status,
            companies: [status],
            canGoBack: false,
          });
        } catch (err) {
          set(connectionError(client, err, defaultCompany));
        }
        return;
      }

      try {
        const companies = await client.listCompanies();
        if (companies.length === 1) {
          const c = companies[0];
          set({ kind: "console", company: c.id, status: c, companies, canGoBack: false });
        } else if (companies.length > 1) {
          set({ kind: "picker", companies });
        } else {
          set({ kind: "no-company" });
        }
      } catch (listErr) {
        // Fall back to the single-company alias (prosumer serve).
        try {
          const status = await client.status(null);
          set({ kind: "console", company: null, status, companies: [], canGoBack: false });
        } catch {
          set(connectionError(client, listErr, defaultCompany));
        }
      }
    }

    void boot();
    return () => {
      cancelled = true;
    };
  }, [client, defaultCompany, forceLogin, bootEpoch]);

  const switchCompany = useCallback(
    async (id: string, companies: CompanyStatus[], knownStatus?: CompanyStatus) => {
      try {
        // A caller that just provisioned or reconciled `id` already holds a
        // fresh `CompanyStatus` for it — `onCompanyCreated` below is the one
        // today. Re-fetching it here was a second, redundant `client.status`
        // call: on a reset the old company is already archived by this
        // point, so a transient failure on this second lookup alone dropped
        // the phase to a connection error despite the create having fully
        // succeeded, and could undo a successful ambiguous-provision
        // reconciliation (`create-company-dialog.tsx`) by failing its own
        // second lookup right after (codex review on #1828, PR comment
        // 3864628314).
        const status = knownStatus ?? (await client.status(id));
        if (phase.kind === "console" && phase.company !== id) {
          clearEntityHash();
        }
        setPhase({ kind: "console", company: id, status, companies, canGoBack: true });
      } catch (err) {
        setPhase(connectionError(client, err, id));
      }
    },
    [client, phase],
  );

  const backToPicker = useCallback(() => {
    void client
      .listCompanies()
      .then((companies) => setPhase({ kind: "picker", companies }))
      .catch((err: unknown) => setPhase(connectionError(client, err, null)));
  }, [client]);

  // The create/reset dialog's open request (issue #1807). `null` is closed.
  // Owned here because this is the one place that holds the picker/console phase
  // machine and `switchCompany` — the fresh company has to be entered, and on a
  // reset the operator's archived company has to be left, both of which are this
  // component's job.
  const [createRequest, setCreateRequest] = useState<CreateCompanyRequest | null>(null);
  const canCreate = offersCompanyCreation(client);

  // Whichever companies the current phase knows about, so a create can drop the
  // operator into the new one alongside the rest.
  const knownCompanies =
    phase.kind === "console" || phase.kind === "picker" ? phase.companies : [];

  const onCompanyCreated = useCallback(
    (status: CompanyStatus) => {
      // On a reset the dialog has already archived this id; drop it from the
      // list it hands `switchCompany` so the operator doesn't land back on a
      // company that is now gone.
      const archived =
        createRequest?.kind === "reset" ? createRequest.company : undefined;
      // The URL and the persisted profile both name whatever company this
      // connection was scoped to *before* this create/reset — read it before
      // `retargetDefaultCompany` below overwrites it. `retargetDefaultCompany`
      // does not distinguish create from reset, so a plain "New company"
      // triggered from inside an explicit-company console retargets the
      // profile exactly the same way a reset does. Gating the URL fix on
      // `archived` (reset-only) missed that case: the profile moved but the
      // `?company=` link didn't, so the next reload's `findProfile` lookup no
      // longer matched the retargeted profile and minted a duplicate
      // connection back on the company the operator just left (codex review
      // on #1828, PR comment 3864628310).
      const priorDefaultCompany = connection?.defaultCompany ?? null;
      setCreateRequest(null);
      const next = [
        ...knownCompanies.filter((c) => c.id !== status.id && c.id !== archived),
        status,
      ];
      // Recorded before `retargetDefaultCompany` below, whose `reseat` is
      // what triggers the boot effect's reboot this unblocks — see
      // `knownStatusRef`. Harmless to set even when the retarget below is a
      // no-op (a connection that was never company-scoped): nothing reseats,
      // so the boot effect never reruns and never consults it.
      knownStatusRef.current = { company: status.id, status };
      // Retarget an explicit-company connection's boot default, or the next
      // reload asks for the id this create/reset just left. A no-op for a
      // connection that was never company-scoped.
      retargetDefaultCompany(connectionId, status.id);
      // The registry fix above does not reach a `?company=` link's own URL —
      // see `retargetCompanyUrlParam` for why a stale param there still
      // orphans the retargeted profile on the next reload. A no-op when the
      // connection was never company-scoped in the first place — and,
      // symmetrically, when this connection isn't the one the page's URL
      // describes. A restored non-bootstrap profile's `?company=` link (if
      // it even has one live in the address bar right now) names some other
      // connection entirely; retargeting the persisted profile above is
      // sufficient for it, and rewriting the URL here would instead point
      // the address bar at a company the *bootstrap* connection never asked
      // for (issue #1828 comment 3865563560).
      if (isBootstrap && priorDefaultCompany) {
        retargetCompanyUrlParam(priorDefaultCompany, status.id);
      }
      // Enter the new company with the status this call already has —
      // `switchCompany`'s own `knownStatus` short-circuit skips a redundant
      // second `client.status` fetch (PR comment 3864628314).
      void switchCompany(status.id, next, status);
    },
    [connection, connectionId, createRequest, isBootstrap, knownCompanies, switchCompany],
  );

  const createDialog = (
    <CreateCompanyDialog
      client={client}
      request={createRequest}
      onClose={(archivedDuringReset) => {
        // Read before `setCreateRequest(null)` clears it — the id this
        // reset was archiving, needed below to know whether it is what the
        // connection is still scoped to.
        const archivedId = createRequest?.kind === "reset" ? createRequest.company : null;
        setCreateRequest(null);
        // A reset's archive leg landed before the operator backed out of the
        // rest of it (cancelled, or gave up retrying a failed create). The
        // picker's roster and the console's shell both still show the
        // company this just removed, and `useCompany` does not self-correct
        // on a later poll failure — refresh via the same roster read
        // `backToPicker` already uses so the picker drops the archived card
        // and the console leaves a shell that can no longer be trusted
        // (codex review on #1828, PR comment 3863028405).
        if (archivedDuringReset) {
          // The in-memory roster refresh above does not reach the
          // *persisted* bootstrap sources: an explicit-company connection's
          // profile, and any `?company=` link, still name the id this just
          // archived. There is no replacement to retarget to — unlike
          // `onCompanyCreated` — so clear both instead of moving them. Left
          // alone, the next reload takes the explicit-company boot branch
          // straight into an id that no longer exists and lands on a
          // connection error rather than back in the picker (codex review on
          // #1828, PR comment 3864885215).
          if (archivedId && connection?.defaultCompany === archivedId) {
            clearDefaultCompany(connectionId);
            // Same bootstrap-only gate as `onCompanyCreated` above, and for
            // the same reason: a restored non-bootstrap profile's abandoned
            // reset has nothing live in the URL to clear.
            if (isBootstrap) retargetCompanyUrlParam(archivedId, null);
          }
          backToPicker();
        }
      }}
      onCreated={onCompanyCreated}
    />
  );

  const consoleCompany = phase.kind === "console" ? phase.company : null;
  const scope = useMemo(
    () => ({ connection: connectionId, company: consoleCompany }),
    [connectionId, consoleCompany],
  );

  // A 401 must not preempt setup. An instance that has never been configured
  // can have no companies and no users at all, so every authenticated route on
  // it answers 401 — and letting that swap the wizard for a sign-in form would
  // put the operator back at the dead end this flow exists to remove, asking
  // them to authenticate against a roster that does not exist yet.
  if (refused && phase.kind !== "login" && phase.kind !== "setup" && phase.kind !== "no-company") {
    return (
      <ConsoleChrome>
        <Login
          client={client}
          company={defaultCompany}
          notice={notice}
          onSignedIn={reBoot}
        />
      </ConsoleChrome>
    );
  }

  // Every phase but `console` is a full-screen state with no app shell, and so
  // no sidebar header for the host switcher to live in. `ConsoleChrome` puts it
  // back — otherwise a host that cannot be reached is a screen with no way off
  // it, which is exactly the dead end the rail's permanent presence prevented.
  switch (phase.kind) {
    case "loading":
      return (
        <FullScreen>
          <div className="flex items-center gap-2 text-sm text-muted-foreground">
            <Loader2 className="size-4 animate-spin" /> Connecting…
          </div>
        </FullScreen>
      );

    case "setup":
      return (
        <ConsoleChrome>
          <SetupWizard client={client} onDone={reBoot} expectsShellRemount />
        </ConsoleChrome>
      );

    case "login":
      return (
        <ConsoleChrome>
          <Login
            client={client}
            company={phase.company}
            notice={phase.notice}
            onSignedIn={reBoot}
          />
        </ConsoleChrome>
      );

    // The failure of ONE host, said to somebody who may hold several.
    //
    // Both halves of this used to address a console with exactly one host, and
    // both were wrong the moment there were two (issue #1358). The hint is the
    // single-host boot message — telling an operator who reached this screen by
    // picking a row out of a switcher to "set the host with ?api=" asks them to
    // configure a host they already configured. And Retry was the only way off
    // the screen, so a host that is simply gone had no disposal: the switcher
    // above can move them elsewhere, but the dead row stays in the roster
    // forever.
    //
    // `connections.length` is what separates the two situations, and it is
    // known here rather than in `connectionError` — which sees a client and an
    // error, not a roster.
    case "error": {
      const alone = connections.length <= 1;
      return (
        <FullScreen>
          <div className="w-full max-w-md space-y-4" data-testid="connection-error">
            <Alert variant="destructive">
              <AlertTitle>Can&apos;t connect</AlertTitle>
              <AlertDescription>
                {phase.message}
                {alone && phase.hint && (
                  <span className="mt-1 block font-mono text-xs opacity-80">{phase.hint}</span>
                )}
              </AlertDescription>
            </Alert>
            <div className="flex gap-2">
              <Button className="flex-1" onClick={() => location.reload()}>
                Retry
              </Button>
              {/* Forgetting is local to this client — the host itself is
                  untouched — so it is offered beside Retry rather than behind
                  Manage hosts, which is two screens away from the failure it
                  answers. Never when it is the only host: a console with no
                  connections at all is a worse place to be left than one with a
                  host that is down. */}
              {!alone && (
                <Button
                  variant="outline"
                  className="flex-1"
                  data-testid="connection-error-forget"
                  onClick={() => onRemoveHost(connectionId)}
                >
                  Forget this host
                </Button>
              )}
            </div>
          </div>
        </FullScreen>
      );
    }

    case "no-company":
      return (
        <FullScreen>
          <div className="w-full max-w-md space-y-4" data-testid="no-company">
            <Alert variant="destructive">
              <AlertTitle>No companies are running on this host</AlertTitle>
              <AlertDescription>
                Start one from setup, or with{" "}
                <span className="font-mono text-xs">opencompany serve --company &lt;dir&gt;</span>.
              </AlertDescription>
            </Alert>
            {/* A fully-archived host lands here. Offer New company so it isn't a
                dead end — but only when this console can actually provision;
                otherwise say why and leave setup as the way forward. */}
            {canCreate && (
              <Button
                className="w-full"
                onClick={() => setCreateRequest({ kind: "create" })}
                data-testid="no-company-new"
              >
                New company
              </Button>
            )}
            <Button
              variant={canCreate ? "outline" : "default"}
              className="w-full"
              onClick={() => setPhase({ kind: "setup" })}
            >
              Open setup
            </Button>
            {!canCreate && (
              <p className="text-2xs text-muted-foreground">{CREATE_UNAVAILABLE_NOTE}</p>
            )}
          </div>
          {createDialog}
        </FullScreen>
      );

    case "picker":
      return (
        <ConsoleChrome>
          <CompanyPicker
            companies={phase.companies}
            onPick={(id) => void switchCompany(id, phase.companies)}
            onCreate={() => setCreateRequest({ kind: "create" })}
            onReset={(c) => setCreateRequest({ kind: "reset", company: c.id, name: c.name })}
            canCreate={canCreate}
          />
          {createDialog}
        </ConsoleChrome>
      );

    case "console":
      return (
        // The remount key carries the connection as well as the company. It was
        // already the isolation primitive — a company switch throws away every
        // piece of in-flight view state rather than reconciling it — and the
        // comments in `app-shell.tsx` about "another company's channel ids are
        // another namespace" are only true again once the host is in the key
        // too. Two hosts each serving an `acme` are two namespaces.
        <ConnectionScopeProvider scope={scope}>
          <AppShell
            key={`${connectionId}:${phase.company ?? "single"}`}
            client={client}
            company={phase.company}
            initialStatus={phase.status}
            companies={phase.companies}
            onSwitchCompany={(id) => void switchCompany(id, phase.companies)}
            onBackToPicker={phase.canGoBack ? backToPicker : undefined}
            onCreateCompany={() => setCreateRequest({ kind: "create" })}
            onResetCompany={(id, name) =>
              setCreateRequest({ kind: "reset", company: id, name })
            }
          />
          {createDialog}
        </ConnectionScopeProvider>
      );
  }
}

function clearEntityHash() {
  const [path] = window.location.hash.replace(/^#\/?/, "").split("?");
  const [view, sub] = path.split("/").filter(Boolean);
  if (!view || !sub) return;

  // A hash sub-route names an entity within the current company. The shell
  // remounts for a company switch, so remove that stale identity before the
  // new shell reads the route as an intentional deep link.
  //
  // The connection scope survives it: the company changed, the host did not,
  // and a `replaceState` fires no `hashchange` for `useHostAddress` to put it
  // back with (`use-host-route.ts`).
  window.history.replaceState(null, "", withHostParam(view));
}

/**
 * A full-screen state, with the host switcher over it.
 *
 * Every caller here is a phase with no app shell, and every one of them is a
 * state an operator may need to leave by choosing a different host — so the
 * chrome is part of the wrapper rather than something each case remembers.
 */
function FullScreen({ children }: { children: React.ReactNode }) {
  return (
    <ConsoleChrome>
      <div className="grid min-h-svh place-items-center bg-background p-6 text-center">
        {children}
      </div>
    </ConsoleChrome>
  );
}

function connectionError(
  client: OpenCompanyClient,
  err: unknown,
  company: string | null,
): Phase {
  const where = client.baseUrl || "this origin";
  if (err instanceof ApiError && err.status === 401) {
    // A 401 now usually means "no session", not "no operator token" — humans
    // sign in. Offering the login view is right for a user and harmless for an
    // operator, who can still pass ?token=.
    return { kind: "login", company };
  }
  return {
    kind: "error",
    message: `Couldn't reach a company host at ${where}.`,
    hint: "Set the host with ?api=<url>, or run `opencompany serve`.",
  };
}
