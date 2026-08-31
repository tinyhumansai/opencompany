import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { Info, ShieldCheck } from "lucide-react";
import { toast } from "sonner";

import { me as fetchMe } from "@/api/auth";
import type { OpenCompanyClient } from "@/api/client";
import {
  disconnectComposioConnection,
  getComposioStatus,
  listComposioConnections,
  startComposioAuthorize,
  type ComposioConnectedAccount,
  type ComposioStatus,
} from "@/api/composio";
import type { ConnectionState } from "@/api/types";
import { PageHeader } from "@/components/page-header";
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Badge } from "@/components/ui/badge";
import { catalogWarning } from "@/lib/composio-catalog";
import { toolkitSlug, type ComposioReach } from "@/lib/connections";
import { classifyLoadFailure } from "@/lib/section-load";
import {
  buildGridProviders,
  connectedProviderCount,
  disconnectRouteFor,
  type GridProvider,
} from "@/lib/provider-grid";
import { ProviderDetail, type ConnectionSubject } from "@/views/connections/ProviderDetail";
import { grantNamespace } from "@/components/grant-namespace";
import { AccountChoiceSection } from "@/views/connections/AccountChoiceSection";
import { CompanyCredentialCard } from "@/views/connections/CompanyCredentialCard";
import { ComposioSection } from "@/views/connections/ComposioSection";
import { ProvidersSection } from "@/views/connections/ProvidersSection";

interface Props {
  client: OpenCompanyClient;
  company: string | null;
}

type Load = "loading" | "ready" | "unavailable";

/**
 * How long the Composio status probe may take before the grid stops waiting on
 * it. Mirrors the host's own `COMPOSIO_PROBE_TIMEOUT` on the connections read
 * path (`src/server/ops/connections_read.rs`) — the same argument applies on
 * this side of the wire: the answer it contributes is which route a tile takes,
 * and a page that paints honestly beats a page that never paints.
 */
const COMPOSIO_PROBE_TIMEOUT_MS = 5_000;

/**
 * The value the probe race resolves when the timeout wins (issue #1478). A
 * unique sentinel, so "the probe timed out" is distinguishable from "the probe
 * answered `null`" — the two are different facts and must render differently.
 */
const PROBE_TIMED_OUT = Symbol("composio-probe-timed-out");

/**
 * OAuth: the third-party accounts this company signs in to and acts through.
 *
 * One question per page. This page used to be "Connections" and carried five
 * unrelated ones — the accounts below, the MCP tool servers, which model the
 * company thinks with, its Telegram channel and its bound repositories — so
 * every one of them was something an operator scrolled past on the way to
 * another. MCP (`#/settings/mcp`) and inference (`#/settings/inference`) are
 * their own pages now; channels and repositories left the product entirely.
 */
export function OAuthView({ client, company }: Props) {
  const [load, setLoad] = useState<Load>("loading");
  const [states, setStates] = useState<Record<string, ConnectionState>>({});
  // The Composio connection objects behind those booleans, keyed by normalized
  // toolkit slug (issue #404). `states` still decides *whether* a provider is
  // connected — this decides *what* is connected, which is what a revoke has to
  // be addressed to and what the detail view opens.
  const [accounts, setAccounts] = useState<Record<string, ComposioConnectedAccount[]>>({});
  // The slug of the provider whose detail view is open, or `null`. A slug rather
  // than the row itself, so the open panel re-derives from `providers` after a
  // refresh instead of showing a snapshot of the state before the revoke.
  const [opened, setOpened] = useState<string | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  // Bumped when the company credential changes, to remount the sections whose
  // reported tier is downstream of it (issue #586).
  const [credentialGeneration, setCredentialGeneration] = useState(0);
  // Whether this viewer may change what the company connects through (issue
  // #403). A connection belongs to the company, so changing one is an admin's
  // call; reading the page is everyone's.
  //
  // **Courtesy, not enforcement.** The host refuses every write on this page
  // with a 403 whatever this says. All hiding the controls prevents is offering
  // an operator a button that cannot work — which on this page would be a
  // particularly poor greeting, since the failure arrives only after they have
  // pasted a live credential into a form that could never submit it.
  const [canManage, setCanManage] = useState(false);
  // The host's Composio answer, or `null` while unknown / not reachable.
  //
  // The whole status, not just the routing facts it used to be narrowed to: the
  // page's one provider grid is built from `effectiveCatalog` (issue #582), and
  // the catalog's honesty markers (`catalogSource`, `catalogNotice`) travel with
  // it. Narrowing here and re-fetching the same call elsewhere for the rest is
  // how the page ended up with two lists.
  const [status, setStatus] = useState<ComposioStatus | null>(null);
  // Slugs connected through the by-slug escape hatch this session, so they keep
  // a tile instead of vanishing after a successful sign-in (issue #397).
  const [extraToolkits, setExtraToolkits] = useState<string[]>([]);
  // Whether this instance carries a platform-projected identity, as Composio
  // reports it. A second witness for the same host-level fact the connection
  // rows carry — and the only one available when the manifest declares no
  // connections, which is exactly when the #319 guard used to go dark.
  const [attested, setAttested] = useState(false);
  // Whether the Composio probe has answered. The grid must not paint before it
  // has: `refresh()` routinely resolves first, and a tile rendered on a null
  // `reach` reads "Not available on this host" — so every tile would flash that
  // and then flip to Connect a moment later.
  const [reachSettled, setReachSettled] = useState(false);
  // Whether the Composio probe LOST ITS RACE against the timeout (issue #1478).
  // Distinct from `status === null`, which is also what a genuine "no Composio
  // surface" answer sets — collapsing the two rendered a timed-out probe as a
  // confident "this host has no providers to offer yet". A slow host is the
  // common case here: the probe reaches an external catalog.
  const [probeFailed, setProbeFailed] = useState(false);
  // Poll timers for Composio sign-ins in flight, keyed by toolkit, so a company
  // switch or unmount cannot leave one running.
  const pollTimers = useRef<Record<string, number>>({});

  // Bumped on every reconciled re-read, so the account-choice section re-reads
  // with it: connecting a second Gmail is exactly when that section appears,
  // and releasing one is exactly when it stops being a choice (issue #820).
  const [connectionsGeneration, setConnectionsGeneration] = useState(0);

  // Issue #1796: whether the `composio` grant is in flight. The grid renders the
  // control, this owns the write — the grid is presentational by design, and
  // giving it host state back is how the page came to have two provider lists.
  const [grantingComposio, setGrantingComposio] = useState(false);

  const refresh = useCallback(async () => {
    // Both reads, together: the page's status and the accounts behind it are one
    // answer to the operator, and refreshing them apart is how a tile ends up
    // connected with nothing to open, or openable after the last account was
    // revoked. Independently faulted, though — the accounts read is a strict
    // addition, and a host that cannot answer it must not cost the page its
    // status.
    const [list, rows] = await Promise.all([
      client.listConnections(company).then(
        (l) => ({ ok: true as const, l }),
        () => ({ ok: false as const, l: [] as ConnectionState[] }),
      ),
      // 409 on a build without Composio, 404 on a host predating #696. Neither
      // is an error for this page: it means there are no connection objects to
      // open, which the empty map already says.
      listComposioConnections(client, company).catch(() => []),
    ]);
    setAccounts(
      Object.fromEntries(
        rows.filter((r) => r.accounts?.length).map((r) => [toolkitSlug(r.toolkit), r.accounts!]),
      ),
    );
    // Bumped here rather than on the success path: the account-choice section
    // is downstream of the accounts read, which just landed, and it is a strict
    // addition to a page whose status read is allowed to fail independently
    // (issue #820). This used to be a `finally` on a `try`; #819 replaced the
    // try/catch with the fault-isolated `Promise.all` above, and the bump has
    // to survive the early return below.
    setConnectionsGeneration((n) => n + 1);
    if (!list.ok) {
      // No connections surface on this host yet — show the catalog read-only.
      setLoad("unavailable");
      return;
    }
    setStates(Object.fromEntries(list.l.map((c) => [c.provider, c])));
    setLoad("ready");
  }, [client, company]);

  useEffect(() => {
    setLoad("loading");
    void refresh();
  }, [refresh]);

  // Composio status drives the only route this page offers (issue #822). A host
  // without the feature, without the grant, or without a credential simply
  // leaves `reach` null, and `connectRoute` falls back to managed/unavailable —
  // there is no native arm to fall back to any more.
  useEffect(() => {
    let live = true;
    setReachSettled(false);
    setProbeFailed(false);
    void (async () => {
      try {
        // Bounded: the shared client has no abort or timeout, and the grid now
        // waits on this call before painting — so a host that accepts the
        // connection and never answers would hold the page on skeletons forever.
        // The timeout resolves a distinct SENTINEL rather than `null`, because a
        // null answer and a request that never answered are different facts: the
        // former is "no Composio surface", the latter is "we could not check",
        // and rendering the second as the first is the #1478 defect.
        const probed = await Promise.race([
          getComposioStatus(client, company),
          new Promise<typeof PROBE_TIMED_OUT>((resolve) =>
            window.setTimeout(() => resolve(PROBE_TIMED_OUT), COMPOSIO_PROBE_TIMEOUT_MS),
          ),
        ]);
        if (!live) return;
        if (probed === PROBE_TIMED_OUT) {
          // Could not check — say so downstream rather than assembling a
          // confident empty state from a request that never answered.
          setStatus(null);
          setAttested(false);
          setProbeFailed(true);
        } else {
          setStatus(probed);
          setAttested(probed?.credentialSource === "attested");
        }
      } catch (err) {
        // A 404 is the honest "no Composio surface on this host" (issue #822):
        // leave the page in its no-route state. Anything else — a 5xx, an
        // offline network, an expired session — is UNKNOWN, not absent, so it
        // takes the same `probeFailed` path as the timeout above. Collapsing it
        // to a confident empty catalog is the #1478 defect one level down.
        if (live) {
          setStatus(null);
          setAttested(false);
          if (classifyLoadFailure(err) === "error") setProbeFailed(true);
        }
      } finally {
        if (live) setReachSettled(true);
      }
    })();
    return () => {
      live = false;
    };
    // `credentialGeneration` is load-bearing, not decoration: this probe feeds
    // `reach.hasCredential` and `attested`, both of which are downstream of the
    // company credential. Setting a key flips `credentialSource` from `none` to
    // `company`, and without a re-probe the grid would keep every tile on the
    // "no credential" route while the Composio section right below it correctly
    // reported the opposite (issue #586).
  }, [client, company, credentialGeneration]);

  useEffect(() => {
    const timers = pollTimers.current;
    return () => {
      Object.values(timers).forEach((id) => window.clearTimeout(id));
      pollTimers.current = {};
    };
  }, [company]);

  useEffect(() => {
    let live = true;
    void (async () => {
      let admin = false;
      try {
        admin = (await fetchMe(client, company)).role === "admin";
      } catch {
        // No user plane on this host, or not signed in — treat as non-admin.
      }
      if (live) setCanManage(admin);
    })();
    return () => {
      live = false;
    };
  }, [client, company]);

  /**
   * The hosted route: Composio runs the OAuth on its own side, so there is no
   * local callback to wait on. Open its connect URL in a tab and poll this
   * company's connection list until the toolkit flips, mirroring
   * `ComposioSection.signIn`.
   *
   * `busy` is cleared by the poll rather than by the caller — the connect is not
   * finished when the tab opens, and clearing early would offer a second Connect
   * for a sign-in already in flight.
   *
   * What the poll waits for is a **new account id**, not `connected` (issue
   * #404). Adding a second account to a toolkit that is already connected leaves
   * `connected` true throughout, so the old predicate was satisfied by the state
   * the operator started from: the first tick — two seconds in, with the
   * Composio tab still open — would have announced a sign-in that had not
   * happened. Comparing ids answers both cases with one rule, since the first
   * connect starts from an empty set.
   */
  async function connectComposio(p: { providerId: string; label: string }, toolkit: string) {
    // A sign-in for this toolkit is already polling (it can have been started
    // from a different tile sharing the slug). Clear the flag we just set rather
    // than leaving this tile spinning on someone else's flow.
    if (pollTimers.current[toolkit] !== undefined) {
      setBusy((b) => (b === p.providerId ? null : b));
      return;
    }
    const { connectUrl } = await startComposioAuthorize(client, company, toolkit);
    // `noopener` keeps the Composio tab from reaching back through
    // `window.opener` — it is a third-party page carrying an OAuth flow, so it
    // stays. The cost is that the handle is ALWAYS null: with `noopener` (or
    // `noreferrer`) set, `window.open` returns null on success exactly as it
    // does when a popup is blocked.
    //
    // So a null check here cannot detect a blocked popup — it fires on every
    // successful open. This was reviewed as "handle a blocked popup", written
    // that way, and caught in manual testing: the tab opened and the operator
    // was told it had not. Detecting the block would mean dropping `noopener`
    // and trading a real security property for a nicer error, which is the
    // wrong trade on a tab we hand an OAuth URL to. `ComposioSection.signIn`
    // opens the same URL the same way and likewise does not check.
    window.open(connectUrl, "_blank", "noopener,noreferrer");
    toast.message(`Complete ${p.label} sign-in in the new tab.`);
    // What was already there before the tab opened. Read from the page's own
    // state rather than re-fetched: it is the same list the operator is looking
    // at, and a fresh read here could race the sign-in they have already
    // completed in another tab.
    const before = new Set((accounts[toolkitSlug(toolkit)] ?? []).map((a) => a.id));
    const wasConnected = providers.some((row) => row.slug === toolkitSlug(toolkit) && row.connected);
    const deadline = Date.now() + 120_000;
    const poll = async () => {
      delete pollTimers.current[toolkit];
      if (Date.now() > deadline) {
        setBusy((b) => (b === p.providerId ? null : b));
        toast.message(`${p.label} sign-in timed out. Try again if it didn't complete.`);
        return;
      }
      try {
        const rows = await listComposioConnections(client, company);
        const row = rows.find((r) => r.toolkit.toLowerCase() === toolkit.toLowerCase());
        // An id we had not seen settles it. Falling back to `connected` covers a
        // host predating the `accounts` field, where the first connect is the
        // only one this poll can observe at all.
        const arrived = row?.accounts?.some((a) => a.connected && !before.has(a.id)) === true;
        if (arrived || (row?.accounts === undefined && row?.connected === true && !wasConnected)) {
          setBusy((b) => (b === p.providerId ? null : b));
          toast.success(`Connected ${p.label}.`);
          // Re-read the host's reconciled view so the tile flips to connected.
          // This is the page's only status source now (issue #582), so one
          // refresh updates everything that speaks about this provider — there
          // is no second list left to go stale behind it.
          await refresh();
          return;
        }
      } catch {
        // Ignore transient probe errors while the operator finishes sign-in.
      }
      pollTimers.current[toolkit] = window.setTimeout(() => void poll(), 2_000);
    };
    pollTimers.current[toolkit] = window.setTimeout(() => void poll(), 2_000);
  }

  async function connect(p: GridProvider) {
    if (busy) return;
    setBusy(p.providerId);
    try {
      if (p.route.kind === "composio") {
        await connectComposio(p, p.route.toolkit);
        return;
      }
      // `managed` / `unavailable` render no Connect button, so reaching here
      // would be a rendering bug rather than an operator action. Composio is
      // the only route left: the native hatch stopped being offered with #822,
      // because what it stores is read by no agent tool (#396).
      setBusy(null);
    } catch {
      toast.error(`Couldn't start the ${p.label} connection.`);
      setBusy(null);
    }
  }

  /**
   * Sign in to a toolkit typed into the by-slug hatch.
   *
   * It has no tile until the host reports it, so it is added to `extraToolkits`
   * first — `buildGridProviders` gives it one from local metadata, and the
   * refresh after a successful poll then fills in the real status.
   */
  async function connectSlug(slug: string) {
    if (busy || !slug) return;
    setExtraToolkits((t) => (t.includes(slug) ? t : [...t, slug]));
    setBusy(slug);
    try {
      await connectComposio({ providerId: slug, label: slug }, slug);
    } catch {
      toast.error(`Couldn't start the ${slug} connection.`);
      setBusy(null);
    }
  }

  /**
   * Revoke one Composio account (issue #404).
   *
   * The only route that releases a Composio connection. `disconnectConnection`
   * — which this page used to call for *every* tile — posts to
   * `…/connections/{provider}/disconnect` and blanks the host's own
   * `oauth/{provider}` secret, which a Composio-connected provider never had:
   * it answered 200, the toast said "Disconnected Gmail", and Gmail was still
   * connected on the next refresh.
   */
  async function disconnectAccount(p: GridProvider, account: ComposioConnectedAccount) {
    if (busy) return;
    setBusy(p.providerId);
    try {
      const { note } = await disconnectComposioConnection(client, company, account.id);
      // The host's own words rather than ours: it is the side that knows what a
      // revoke reached, and restating it here is a second place to drift from
      // what actually happened.
      toast.success(`${account.account ?? p.label}: ${note}`);
      await refresh();
    } catch {
      toast.error(`Couldn't disconnect ${account.account ?? p.label}.`);
    } finally {
      setBusy(null);
    }
  }

  /** Blank the host's own `oauth/{provider}` secret — the self-hosted route. */
  async function disconnectNative(p: GridProvider) {
    if (busy) return;
    setBusy(p.providerId);
    try {
      await client.disconnectConnection(p.providerId, company);
      toast.success(`Disconnected ${p.label}.`);
      await refresh();
    } catch {
      toast.error(`Couldn't disconnect ${p.label}.`);
    } finally {
      setBusy(null);
    }
  }

  /**
   * The tile's Disconnect — the native route, and only ever the native route.
   *
   * Routed rather than assumed. A tile backed by Composio opens instead of
   * offering this (see `ProviderTile`), so in practice nothing reaches the other
   * arm; the check is here because the alternative is the defect this whole
   * change is about. Sending a Composio provider to
   * `…/connections/{provider}/disconnect` blanks a secret it never had and
   * answers 200, and that failure is invisible at the call site — it looks like
   * a working disconnect until the next refresh. A rule that cannot be
   * mis-called beats a comment asking callers not to.
   *
   * Falling back to opening the panel rather than throwing: a toolkit's accounts
   * are revoked one at a time, by id, and the panel is where they are named.
   * Picking one here — first, newest, all — would revoke an account the operator
   * never saw.
   */
  async function disconnect(p: GridProvider) {
    const route = disconnectRouteFor(p);
    if (route === null) return;
    if (route.kind === "composio") {
      setOpened(p.slug);
      return;
    }
    await disconnectNative(p);
  }

  // `attested` is a property of the *instance*, not of one provider: it means
  // this pod carries a platform-minted identity, so every connection is the
  // platform's to run. One row reporting it is therefore enough to say so once
  // at the top, rather than only in per-tile copy (issue #319).
  //
  // Read from the Composio status as well as the connection rows (issue #599).
  // `GET …/connections` only answers for providers the manifest declares, so a
  // tenant that declares none produced no rows at all — and this went false on a
  // host that is unambiguously platform-managed, which is how eleven tiles ended
  // up offering a Connect that could only 400.
  const platformManaged =
    load === "ready" &&
    (Object.values(states).some((s) => s.credentialSource === "attested") || attested);

  // The routing facts, narrowed out of the status the page already holds.
  const reach: ComposioReach | null = useMemo(
    () =>
      status
        ? {
            inBuild: status.inBuild,
            granted: status.granted,
            hasCredential: status.credentialSource !== "none",
            openMode: status.openMode,
            effectiveToolkits: status.effectiveToolkits,
          }
        : null,
    [status],
  );

  // The page's one provider list. Status, route and metadata resolved together,
  // once, so nothing on this screen can answer differently from anything else
  // on it (issue #582).
  const providers = useMemo(
    () =>
      buildGridProviders(
        status?.effectiveCatalog ?? [],
        extraToolkits,
        states,
        reach,
        platformManaged,
        accounts,
      ),
    [status?.effectiveCatalog, extraToolkits, states, reach, platformManaged, accounts],
  );

  // Re-derived from the grid every render, so the open panel reflects the last
  // refresh rather than the row as it was when it was clicked. A provider that
  // vanishes from the catalog under an open panel closes it, which is the only
  // honest thing left to show.
  const openedProvider = useMemo(
    () => providers.find((p) => p.slug === opened) ?? null,
    [providers, opened],
  );

  // The panel's subject, with this arm's controls attached (issue #821). A
  // Composio provider cannot be opened without the revoke that actually
  // releases it — `disconnectConnection` answers 200 while releasing nothing,
  // which is the defect `disconnect()` above exists to route around — so the
  // handlers travel inside the variant rather than as sibling props.
  //
  // Rebuilt every render rather than memoized: nothing downstream keys off this
  // object's identity (the panel's usage read keys off the slug it derives),
  // and a memo whose value carries fresh closures would have to lie about its
  // dependencies to stay stable.
  const detailSubject: ConnectionSubject | null =
    openedProvider === null
      ? null
      : {
          kind: "composio",
          provider: openedProvider,
          noCredential: status?.credentialSource === "none",
          onConnectAnother: (p) => void connect(p),
          onDisconnectAccount: (p, account) => void disconnectAccount(p, account),
        };

  // Counted off the rendered grid, not off the raw host rows. The badge used to
  // count every row `GET …/connections` returned while the grid below drew only
  // the eleven tiles the console had metadata for — so the header could read "3
  // connected" above a grid showing none of them (issue #582). Derived through
  // the shared helper so this and the section heading's own count cannot drift
  // (issue #1407).
  const connectedCount = connectedProviderCount(providers);

  return (
    <div className="flex min-h-0 flex-1 flex-col">
      <PageHeader
        title="OAuth"
        width="5xl"
        description={
          <>
            The third-party accounts your company signs in to and acts through. It only uses
            what you connect.
          </>
        }
        trailing={
          load === "ready" && connectedCount > 0 ? (
            <Badge variant="secondary">{connectedCount} connected</Badge>
          ) : null
        }
      />
      <div className="mx-auto min-h-0 w-full max-w-5xl flex-1 space-y-6 overflow-y-auto px-4 py-6">
        {load === "unavailable" && (
          <Alert>
            <Info className="size-4" />
            <AlertTitle>OAuth connections aren&apos;t wired on this host yet</AlertTitle>
            <AlertDescription>
              The catalog below shows what your company can connect once the host exposes its OAuth
              endpoints. Connecting is disabled until then.
            </AlertDescription>
          </Alert>
        )}

        {platformManaged && (
          <Alert data-testid="connections-platform-managed">
            <ShieldCheck className="size-4" />
            <AlertTitle>Connections are managed by the platform</AlertTitle>
            <AlertDescription>
              This instance signs in with its own platform identity, so there is no provider key to
              register here and nothing stored on this instance. Connect a provider from the
              platform and it shows up here.
            </AlertDescription>
          </Alert>
        )}

        {!canManage && (
          <Alert data-testid="connections-read-only">
            <Info className="size-4" />
            <AlertTitle>Only an admin can change what this company connects through</AlertTitle>
            <AlertDescription>
              A connection belongs to the company — it is the account your teammates act
              through — so an admin manages it. You can see everything that is wired here; ask an
              admin to add, change or remove one.
            </AlertDescription>
          </Alert>
        )}

        {/* The general answer sits above the Composio-specific one: this key
            authorizes every brokered surface, and the Composio token below is
            the escape hatch (issue #586). */}
        <CompanyCredentialCard
          client={client}
          company={company}
          canManage={canManage}
          onChanged={() => setCredentialGeneration((n) => n + 1)}
        />

        {/* Remounted on a credential change so its status is re-read: the tier
            it reports (`company` vs `attested` vs `none`) is downstream of the
            key that was just set, and a stale badge would tell the operator
            their change did not land. */}
        <ComposioSection
          key={credentialGeneration}
          client={client}
          company={company}
          canManage={canManage}
          onChanged={() => setCredentialGeneration((n) => n + 1)}
        />

        {/* The page's one provider list (issue #582). It used to be two — this
            grid and a categorised grid of eleven hardcoded tiles below it — and
            they disagreed by construction, so a provider could show as connected
            in one and offer a Connect button in the other on the same screen. */}
        <ProvidersSection
          providers={providers}
          canManage={canManage && load !== "unavailable"}
          busy={busy}
          noCredential={status?.credentialSource === "none"}
          granted={status?.granted}
          probeFailed={probeFailed}
          openMode={status?.openMode === true}
          degraded={status ? catalogWarning(status) : null}
          loading={load === "loading" || !reachSettled}
          granting={grantingComposio}
          onGrant={() => {
            void (async () => {
              setGrantingComposio(true);
              try {
                // Both generations bump: the grant changes what the page's
                // status says about `granted`, and `ComposioSection` reads the
                // same flag one card up. Refreshing one and not the other is
                // the two-surfaces-disagreeing failure #582 is about.
                if (await grantNamespace(client, company, "composio")) {
                  setCredentialGeneration((n) => n + 1);
                  await refresh();
                }
              } finally {
                setGrantingComposio(false);
              }
            })();
          }}
          onConnect={(p) => void connect(p)}
          onDisconnect={(p) => void disconnect(p)}
          onOpen={(p) => setOpened(p.slug)}
          onConnectSlug={(slug) => void connectSlug(slug)}
        />

        {/* Only renders for a provider this company holds two or more accounts
            for — the one case where "which account do teammates act as" is a
            question the product can answer (issue #820). */}
        <AccountChoiceSection
          client={client}
          company={company}
          canManage={canManage}
          generation={connectionsGeneration + credentialGeneration}
        />

        {/* A connection as an object you open rather than a row with a button
            (issue #404). The grid's half is Composio only: the native catalog is
            inert — its credential is written and read by nothing (#396) — and a
            detail view is exactly where that would look healthiest. The same
            panel opens a remote MCP server, from `McpServersSection` (#821). */}
        <ProviderDetail
          client={client}
          company={company}
          subject={detailSubject}
          canManage={canManage && load !== "unavailable"}
          busy={busy !== null}
          onClose={() => setOpened(null)}
        />
      </div>
    </div>
  );
}
