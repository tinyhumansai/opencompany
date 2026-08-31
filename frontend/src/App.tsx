import {
  lazy,
  Suspense,
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
} from "react";
import { Loader2 } from "lucide-react";

import { signInWithHubToken, verifyCode } from "@/api/auth";
import { isAddressableBaseUrl, isDesktopRuntime } from "@/api/transport";
import {
  createLocalInstance,
  renameLocalInstance,
  deleteLocalInstance,
  embeddedHost,
  localInstances,
  openSshTunnel,
  startLocalInstance,
  stopLocalInstance,
  type LocalInstance,
} from "@/api/transport/desktop";
import { ApiError } from "@/api/types";
import { ConsoleChrome } from "@/components/host-switcher";
import { ManageHostsPage } from "@/components/manage-hosts";
import { RouteLoading } from "@/components/route-loading";
import { Button } from "@/components/ui/button";
import { resolveConfig } from "@/config";
import {
  addConnection,
  adoptLocalHosts,
  clientFor,
  editConnection,
  listConnections,
  probe,
  removeConnection,
  restoreConnections,
  useConnections,
} from "@/connections/registry";
import { HostsProvider, useHosts, type HostsValue } from "@/connections/HostsContext";
import { firstHostCopy } from "@/connections/first-host";
import type { ConnectionId } from "@/connections/types";
import { useHostAddress, useHostRoute } from "@/hooks/use-host-route";
import { absorbHubSetupHandoff } from "@/setup/state";
import { ConnectionConsole } from "@/views/ConnectionConsole";
import { AddHostPage } from "@/views/setup/AddHostPage";
import { cn } from "@/lib/utils";

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
 *
 * `company` is deliberately kept, for the same reason `clearHubResultFromUrl`
 * keeps it: it is not a credential, and it is what scopes the console. This
 * once deleted it too — harmless back when the console was one implicit host,
 * and a silent state reset once connections arrived. `restoreConnections` is
 * told which same-origin console this load is (`isThisConsole`, added for
 * #1167), and on a stripped URL that is `defaultCompany === null` — so the
 * profile the link had just written is skipped, and `addConnection` mints a
 * *second* id for the one host. Every key named after that id starts over: the
 * tour, unread counts, the last-visited channel, the mail draft. The reported
 * symptom was a welcome tour that came back after being skipped; see #1306.
 *
 * The hash is preserved: it is the router's, not ours, and a magic link may
 * carry a deep link to land on.
 */
export function clearMagicLinkFromUrl(): void {
  const params = new URLSearchParams(window.location.search);
  if (!params.has("code")) return;
  params.delete("code");
  const query = params.toString();
  window.history.replaceState(
    {},
    "",
    window.location.pathname + (query ? `?${query}` : "") + window.location.hash,
  );
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
 *
 * The hash is preserved for the same reason it is in `clearMagicLinkFromUrl`:
 * it belongs to the router, and rewriting the URL without it would bounce a
 * deep link back to the default view.
 */
export function clearHubResultFromUrl(): void {
  const params = new URLSearchParams(window.location.search);
  if (params.get("key") !== "auth") return;
  params.delete("token");
  params.delete("error");
  params.delete("key");
  const query = params.toString();
  window.history.replaceState(
    {},
    "",
    window.location.pathname + (query ? `?${query}` : "") + window.location.hash,
  );
}

/**
 * The styleguide answers before anything else does.
 *
 * It reads no company and holds no client — it renders the stylesheet and
 * nothing else — so putting it behind the sign-in gate would only mean that
 * reviewing the design system required credentials for a running host. This
 * way `#/styleguide` works against any build, including a static one.
 *
 * Checked before `resolveConfig()` so a console with no reachable host still
 * serves it.
 */
function isStyleguideRoute(): boolean {
  return window.location.hash.replace(/^#\/?/, "").split("?")[0] === "styleguide";
}

export function App() {
  /**
   * Tracked rather than read once, so `#/styleguide` typed into the address
   * bar of a *running* console switches to it. Without the listener the shell
   * below would keep the screen, and its own router — which does not know
   * this route — would canonicalize the unknown hash straight back to
   * `#/overview`, making the styleguide reachable only by a full reload.
   */
  const [styleguide, setStyleguide] = useState(isStyleguideRoute);
  useEffect(() => {
    const onHash = () => setStyleguide(isStyleguideRoute());
    window.addEventListener("hashchange", onHash);
    return () => window.removeEventListener("hashchange", onHash);
  }, []);

  if (styleguide) {
    return (
      /* `min-h-screen` because this boundary is the whole document — there is
         no console shell around it for `RouteLoading`'s `flex-1` to grow
         inside, and without a height the loading line sits flush against the
         top of the window (measured in Chromium at 1440x900: a 20px-tall
         holder at y=0). Every other route's boundary is already inside the
         shell's flex column, so they pass it nothing and are unchanged. */
      <Suspense
        fallback={
          <div className="flex min-h-screen">
            <RouteLoading title="Styleguide" label="Loading styleguide…" />
          </div>
        }
      >
        <StandaloneStyleguide />
      </Suspense>
    );
  }
  return <Console />;
}

const StandaloneStyleguide = lazy(() =>
  import("@/views/StyleguideView").then((m) => ({ default: m.StyleguideView })),
);

function Console() {
  const config = useMemo(() => resolveConfig(), []);
  /**
   * The bootstrap connection.
   *
   * The web build resolves exactly one, from the same `resolveConfig()` it
   * always did — nothing about how a browser finds its host has changed. What
   * changed is that the host is now a *record* rather than an implicit global,
   * so a second one is an addition rather than a rewrite.
   */
  const bootstrapId = useMemo(
    () => {
      // Hosts added in a previous session come back first, so the bootstrap add
      // below finds its own profile already registered and reuses that entry
      // rather than creating a duplicate row for one host.
      //
      // Told which same-origin console this load is, so the rows a *previous*
      // one left behind stay out of the switcher. A link carrying `?company=`
      // writes its own profile at the same (empty) address, so restoring every
      // one of them put an identical row in the menu for every company ever
      // opened here — issue #1167. Only when the bootstrap is same-origin: a
      // console pointed elsewhere with `?api=` claims nothing about what lives
      // at its own origin.
      restoreConnections(
        undefined,
        config.baseUrl === "" ? { defaultCompany: config.company } : undefined,
      );
      // `null` in the desktop, which has no host at its own origin and never
      // will — see `isAddressableBaseUrl`. Adding one anyway is what made the
      // packaged app open on a connection that could not work and select it,
      // presenting a failure on every launch while the embedded host added
      // below sat healthy and unselected (issue #613).
      //
      // Only the same-origin *default* is refused, not the config path: a
      // desktop given an explicit host through `?api=` or an injected
      // `OPENCOMPANY_CONFIG` still gets its bootstrap connection.
      if (!isAddressableBaseUrl(config.baseUrl)) return null;
      // A hub's own origin serves this bundle and nothing else — there is no
      // host there and there never will be. Adding one anyway reproduces #613
      // in the browser exactly: a connection that cannot work, selected on
      // load, presenting a failure in front of the hosts that are healthy. The
      // hosts a hub knows are the remembered ones, which `restoreConnections`
      // above has already put back.
      //
      // As with the desktop, only the same-origin *default* is refused: a hub
      // opened with an explicit `?api=` still gets its bootstrap connection,
      // which is how a link to one specific host is shared.
      if (config.hub && config.baseUrl === "") return null;
      return addConnection({
        baseUrl: config.baseUrl,
        defaultCompany: config.company,
        credential: config.operatorToken
          ? { kind: "platform", token: config.operatorToken }
          : { kind: "cookie" },
      });
    },
    [config],
  );
  /**
   * The host running inside this application, when there is one.
   *
   * Asked for rather than assumed: the embedded host binds an ephemeral port,
   * so only the core knows its address — and it may not be running at all,
   * most often because another instance holds the data root. `null` then, and
   * the desktop still shows every remote host, which is the point of holding
   * several.
   *
   * Added after the first paint because the address arrives over IPC; the probe
   * effect below picks it up from the id list, so nothing here has to drive it.
   *
   * Registered through `adoptEmbeddedHost` rather than `addConnection`, because
   * this is the one host whose address is *expected* to have changed since last
   * launch — recognising it by that address is what left a dead row behind on
   * every run (#615).
   *
   * Its id is kept because two things need it and neither can find it by
   * sorting: it is what the desktop selects on launch, and — through
   * `resolved` — what tells "not asked yet" apart from "there is no embedded
   * host". Both leave `id` null and they read as opposite things on screen, one
   * a spinner and the other a failure someone has to act on (#613).
   */
  const [embedded, setEmbedded] = useState<EmbeddedState>(() => ({
    // A browser has nothing to ask, so it is resolved before it starts.
    resolved: !isDesktopRuntime(),
    id: null,
    instances: [],
  }));

  /**
   * Asks the core what it is running, and reconciles the connection list.
   *
   * Called on launch and after every start, stop, create and removal, rather
   * than each of those patching a local copy of the roster. The core holds the
   * sockets, so it is the only thing that knows which instances are actually
   * listening — a roster mirrored in React is one that disagrees the first time
   * a start fails.
   */
  const refreshLocal = useCallback(async (): Promise<void> => {
    const instances = await localInstances();
    if (instances === null) {
      // A shell predating the roster. It runs exactly one host and answers only
      // `oc_embedded`, so ask that instead — the degrade is exact, not partial.
      const host = await embeddedHost();
      // Adopted once. A second call would re-run the prune against a set it has
      // already reconciled, for a value this one already has.
      const id = host ? adoptLocalHosts([host])[0] : null;
      setEmbedded({ resolved: true, id, instances: [] });
      return;
    }

    // Only the running ones become connections. A stopped instance has no
    // address, so a row for it could do nothing but fail its probe forever; it
    // is visible — and startable — in the roster instead.
    const running = instances.filter(
      (instance): instance is LocalInstance & { baseUrl: string } =>
        instance.running && typeof instance.baseUrl === "string",
    );
    const ids = adoptLocalHosts(
      running.map((instance) => ({
        baseUrl: instance.baseUrl,
        instanceId: instance.instanceId,
        label: instance.label,
      })),
      // The whole roster, not just the running half. Without it a stopped
      // instance is indistinguishable from a data root this application no
      // longer serves, and the prune forgets its profile — which is the
      // connection id every `scopedKey` under it is named after.
      instances
        .map((instance) => instance.instanceId)
        .filter((id): id is string => id !== undefined),
    );
    setEmbedded({
      resolved: true,
      // The first running instance, which is the one rooted at the data dir on
      // every machine that has not deliberately stopped it. What the desktop
      // opens on when nothing else is selected.
      id: ids[0] ?? null,
      instances,
    });
  }, []);

  useEffect(() => {
    let cancelled = false;
    void refreshLocal().catch((error: unknown) => {
      console.error("[desktop] could not read the local hosts", error);
      if (!cancelled) setEmbedded((prior) => ({ ...prior, resolved: true }));
    });
    return () => {
      cancelled = true;
    };
  }, [refreshLocal]);

  const connections = useConnections();
  /**
   * Which console is on screen.
   *
   * Local UI state, deliberately not in the registry. Every connection stays
   * registered and probed regardless of what is selected — selection changes
   * what is *rendered* and nothing else. A selected-connection field in the
   * registry is the single-valued thing that stops buzz from holding two
   * workspaces, and it would undo this slice.
   *
   * `null` until someone chooses, which is the desktop's ordinary state: it has
   * no bootstrap connection to seed this with. What is on screen then is
   * decided by `active` below, not by leaving this pointing at a host that does
   * not exist.
   *
   * Local, but no longer *only* local: it is seeded from and mirrored into the
   * address, so a switch is a history entry Back can undo and the bar names the
   * host being looked at. That is issue #1358, and `use-host-route.ts` is the
   * whole of it. The registry is still untouched — every connection stays
   * registered and probed regardless of what the address says.
   */
  const { selected, selectHost, resettleHost } = useHostRoute(bootstrapId);

  // A pure read, so StrictMode's double render is harmless.
  const magicLink = useMemo(() => readMagicLink(), []);
  const hubToken = useMemo(() => readHubToken(), []);
  const hubFailed = useMemo(() => readHubError(), []);
  /**
   * The in-flight redemption, so a link is redeemed exactly once.
   *
   * StrictMode double-invokes effects, and a login code is single-use: the
   * second call would spend nothing and 401, bouncing a perfectly good sign-in
   * to the login screen. Both runs await this one promise instead.
   */
  const redemption = useRef<Promise<unknown> | null>(null);
  const [auth, setAuth] = useState<{ ready: boolean; notice?: string; failed?: boolean }>({
    // Nothing to redeem is the common case, and it must not cost a frame.
    ready: !magicLink && !hubToken && !hubFailed,
  });

  // Now that any credential is captured in state, take it out of the URL.
  useEffect(() => {
    if (magicLink) clearMagicLinkFromUrl();
    if (hubToken || hubFailed) {
      clearHubResultFromUrl();
      // A hub sign-in that was asked to land on setup's destination carries it
      // as a query parameter (`?from=setup`) — the host put it there so the
      // OAuth round trip could carry it. Translate it into the hash marker the
      // shell consumes, so the sign-in lands on the roster setup just built
      // with the welcome suppressed, exactly as a setup link would have.
      absorbHubSetupHandoff();
    }
  }, [magicLink, hubToken, hubFailed]);

  /**
   * Redeem a landing credential before any console asks for data.
   *
   * Stays in `App` rather than moving into the console: a magic link arrives on
   * the document URL, which belongs to the app, and it always names the
   * bootstrap connection — there is no way to land on a link for the second
   * host you added yesterday.
   */
  useEffect(() => {
    if (auth.ready) return;
    if (bootstrapId === null) {
      // A landing credential names the bootstrap host, and this runtime has
      // none to redeem it against. Opening the console beats sitting on
      // "Signing in…" forever, waiting on a client that will never exist.
      setAuth({ ready: true });
      return;
    }
    const client = clientFor(bootstrapId);
    if (!client) return;
    let cancelled = false;

    async function redeem() {
      if (hubFailed) {
        if (!cancelled)
          setAuth({
            ready: true,
            failed: true,
            notice: "That sign-in didn't complete. Try again, or use a link below.",
          });
        return;
      }
      if (hubToken) {
        try {
          redemption.current ??= signInWithHubToken(client!, config.company, hubToken);
          await redemption.current;
        } catch (err) {
          if (!cancelled) setAuth({ ready: true, failed: true, notice: hubNotice(err) });
          return;
        }
      }
      if (magicLink) {
        try {
          redemption.current ??= verifyCode(client!, magicLink.company ?? config.company, magicLink.code);
          await redemption.current;
        } catch (err) {
          // A dead link is not fatal — fall through to sign-in and let them ask
          // for another. It has to *say so*, though: `failed` alone only forces
          // the form, and the form a refused link lands on is byte-identical to
          // the one a cold visit gets, so the click reads as having done
          // nothing at all (issue #1305). The reason stays vague about *people*
          // and specific about the *credential* — see `magicLinkNotice`.
          if (!cancelled) setAuth({ ready: true, failed: true, notice: magicLinkNotice(err) });
          return;
        }
      }
      if (!cancelled) setAuth({ ready: true });
    }

    void redeem();
    return () => {
      cancelled = true;
    };
  }, [auth.ready, bootstrapId, config.company, hubFailed, hubToken, magicLink]);

  // Probe every registered connection, independently: one host being slow or
  // unreachable must not hold up another's console.
  //
  // Keyed on the *ids*, not the connection objects. Every status change emits a
  // fresh array, so depending on the array would re-run this on each one — and
  // `probe` itself sets `connecting`, which is a status change. The registry's
  // in-flight guard makes that safe regardless; this keeps it from happening.
  const connectionIds = connections.map((c) => c.id).join(",");
  useEffect(() => {
    // Redemption must land first: a probe before it would read a session that
    // does not exist yet and park the bootstrap row on `unauthenticated`.
    if (!auth.ready) return;
    for (const id of connectionIds.split(",").filter(Boolean)) {
      void probe(id);
    }
  }, [auth.ready, connectionIds]);

  /**
   * Which connection is rendered, in the order these questions get answers.
   *
   * The embedded host comes before "whichever is first" deliberately. Restored
   * hosts are added before it — its port only arrives over IPC — so position in
   * the list is a record of when a host was learned about, not of which one a
   * person opening the desktop means. A launch that lands on someone's remote
   * host because they added it last Tuesday is the same bug as #613 wearing
   * different clothes.
   *
   * Which is also why the last fall-through waits for `resolved`. Until the
   * core answers, "no embedded host" and "not asked yet" are indistinguishable
   * from the list alone, and taking the first entry in the meantime opens a
   * remembered host — mounting its console and issuing its requests — only to
   * replace it a moment later. A brief wrong host is a smaller version of the
   * same bug, so the desktop holds its startup state instead. A browser never
   * waits: `resolved` starts `true` there, because there is nothing to ask.
   */
  const active =
    connections.find((c) => c.id === selected) ??
    connections.find((c) => c.id === embedded.id) ??
    (embedded.resolved ? connections[0] : undefined);
  const client = active ? clientFor(active.id) : undefined;

  /**
   * The address names the host on screen, and only while there is a choice.
   *
   * One host is unambiguous, so its console keeps clean addresses; the scope
   * appears with the second. Declared here, above the sign-in gate, because it
   * is a hook and that gate returns early — and `active` is a pure derivation,
   * so resolving it first costs nothing.
   */
  useHostAddress(active?.id ?? null, connectionIds, connections.length > 1);

  if (!auth.ready) {
    return (
      <FullScreen>
        <Waiting>Signing in…</Waiting>
      </FullScreen>
    );
  }

  // Everything the switcher needs, assembled once and carried down by context.
  //
  // Context rather than props because the switcher now lives in the sidebar
  // header — two layers below here, inside `ConnectionConsole` — and threading
  // eight fields through a component whose job is a phase machine would put the
  // whole roster in the way of every one of its states.
  const hosts: HostsValue = {
    connections,
    selected: active?.id ?? null,
    // A navigation, not a mode change: `selectHost` pushes a history entry, so
    // Back returns to the host that was on screen. `active?.id` rather than
    // `selected` is what the entry being left gets stamped with — the desktop
    // leaves `selected` null until someone chooses, and a stamp of `null` would
    // leave that entry naming nobody.
    onSelect: (id) => selectHost(id, active?.id ?? null),
    onAdd: (baseUrl, connector) => {
      const id = addConnection({ baseUrl, connector });
      selectHost(id, active?.id ?? null);
      void probe(id);
    },
    localInstances: embedded.instances,
    // Only offered where a host can actually be started: the browser build has
    // no core to start one in, and passing handlers it cannot honour would put
    // a button on screen that always fails.
    onAddLocal: isDesktopRuntime()
      ? async (label) => {
          const created = await createLocalInstance(label);
          await refreshLocal();
          // Selected straight away: someone who just created a company means to
          // open it, and the alternative is a new row they have to find.
          if (created.instanceId) {
            const opened = listConnections().find(
              (c) => c.identity?.instanceId === created.instanceId,
            );
            if (opened) selectHost(opened.id, active?.id ?? null);
          }
        }
      : undefined,
    // Only where a process can be started, like the local half above. The
    // tunnel is opened here rather than left to the first probe so that a
    // destination `ssh` refuses is reported in the dialog the operator is
    // standing in front of, instead of becoming a red row they have to go and
    // read. Every *later* launch does it from `probe`, where the address of a
    // remembered tunnel is rebuilt.
    onAddSsh: isDesktopRuntime()
      ? async (target) => {
          const tunnel = await openSshTunnel(target);
          const id = addConnection({
            baseUrl: tunnel.baseUrl,
            // The machine's name, not the loopback port: the port is this
            // launch's and means nothing to the person who typed the other.
            label: target.destination,
            connector: { kind: "ssh", target },
          });
          selectHost(id, active?.id ?? null);
          void probe(id);
        }
      : undefined,
    onStartLocal: isDesktopRuntime()
      ? async (id) => {
          await startLocalInstance(id);
          await refreshLocal();
        }
      : undefined,
    // Setup, finishing, names the host after the company it just built. Matched
    // by instance identity rather than by address, for the reason the registry
    // gives everywhere else: the address is this launch's and the identity is
    // the machine's.
    onNameLocalHost: isDesktopRuntime()
      ? async (label) => {
          const instanceId = active?.identity?.instanceId;
          if (!instanceId) return;
          const local = embedded.instances.find(
            (instance) => instance.instanceId === instanceId,
          );
          // A remote host, or one this shell does not own. Nothing to rename,
          // and nothing wrong: the caller does not know which kind it is on.
          if (!local) return;
          await renameLocalInstance(local.id, label);
          await refreshLocal();
        }
      : undefined,
    onEditHost: (id, change) => editConnection(id, change),
    // Selection has to move *with* the removal, in one step. `active` falls
    // through to the first connection when nothing is selected, so a console
    // whose host has just been forgotten would otherwise render the removed
    // row's client for a frame — and in the desktop, where `selected` is
    // ordinarily null, it would keep rendering whatever came next without ever
    // recording the choice.
    //
    // `resettleHost`, not `selectHost`: a host that has been forgotten is not
    // somewhere Back should be able to return to, and the entry would name a
    // connection this client no longer holds. `useHostAddress` takes the dead
    // id out of the bar for the same reason.
    onRemoveHost: (id) => {
      removeConnection(id);
      const remaining = listConnections();
      resettleHost(
        remaining.some((c) => c.id === selected) ? selected : (remaining[0]?.id ?? null),
      );
    },
    onStopLocal: isDesktopRuntime()
      ? async (id) => {
          await stopLocalInstance(id);
          await refreshLocal();
        }
      : undefined,
    onDeleteLocal: isDesktopRuntime()
      ? async (id) => {
          const instance = embedded.instances.find((candidate) => candidate.id === id);
          const connection = instance?.instanceId
            ? listConnections().find(
                (candidate) => candidate.identity?.instanceId === instance.instanceId,
              )
            : undefined;
          await deleteLocalInstance(id);
          await refreshLocal();

          // A deleted local host is not somewhere Back can return to. The
          // roster refresh prunes its connection; this keeps the selected host
          // and address bar in step with that removal as one user action.
          if (connection?.id === selected) {
            resettleHost(listConnections()[0]?.id ?? null);
          }
        }
      : undefined,
    hub: Boolean(config.hub),
  };

  return (
    <HostsProvider value={hosts}>
      <ConsoleOrAddHost>
        {active && client ? (
          // Keyed by connection: switching hosts remounts rather than
          // reconciling, so no view can carry one host's in-flight state into
          // another's render.
          <ConnectionConsole
            key={active.id}
            connectionId={active.id}
            client={client}
            defaultCompany={active.defaultCompany}
            notice={active.id === bootstrapId ? auth.notice : undefined}
            forceLogin={active.id === bootstrapId && auth.failed === true}
            isBootstrap={active.id === bootstrapId}
          />
        ) : (
          // The switcher rides along, because an operator whose local host is
          // gone still has somewhere else to connect to — and "Add a host" is
          // the only way out of a desktop that holds none.
          <ConsoleChrome>
            <NoConnection starting={!embedded.resolved} desktop={isDesktopRuntime()} />
          </ConsoleChrome>
        )}
      </ConsoleOrAddHost>
      {/* Beside the console for the same reason, and more so: forgetting the
          host on screen selects another one, which remounts the console. A page
          mounted within would unmount itself mid-edit. */}
      <ManageHostsPage />
    </HostsProvider>
  );
}

/**
 * The console, and the add-host screen that stands in front of it.
 *
 * Adding a host is a screen of the onboarding flow rather than a dialog over
 * the console (`views/setup/AddHostPage.tsx`), so it needs somewhere to be
 * drawn that is *outside* the console: creating a host on this computer selects
 * it, and that remounts the console. Drawn within, the screen would take itself
 * off screen at the moment it succeeded.
 *
 * The console is **hidden, not unmounted**, while it is up. The console owns
 * this host's streams and its boot, and someone who opens the chooser and
 * changes their mind should come back to the page they left rather than to
 * "Connecting…".
 *
 * A component of its own only because the flag lives in `HostsContext`, which
 * `App` provides and therefore cannot read.
 */
function ConsoleOrAddHost({ children }: { children: React.ReactNode }) {
  const { addingHost } = useHosts();
  return (
    <>
      <div className={cn("min-h-svh", addingHost && "hidden")}>{children}</div>
      {addingHost ? <AddHostPage /> : null}
    </>
  );
}

/** Where the hosts on this machine got to, and what they turned into. */
interface EmbeddedState {
  /** Whether the core has answered. `false` only ever means "still asking". */
  resolved: boolean;
  /**
   * The connection the *first running* local instance became, or `null` when
   * none is running. What the desktop opens on.
   */
  id: ConnectionId | null;
  /**
   * Every local instance the core knows about, running or not.
   *
   * The stopped ones are here and nowhere else: they have no address, so they
   * cannot be connections. The switcher's "Add a host" screen is where they
   * are startable.
   */
  instances: LocalInstance[];
}

function FullScreen({ children }: { children: React.ReactNode }) {
  return (
    <div className="grid min-h-svh place-items-center bg-background p-6 text-center">
      {children}
    </div>
  );
}

function Waiting({ children }: { children: React.ReactNode }) {
  return (
    <div className="flex items-center gap-2 text-sm text-muted-foreground">
      <Loader2 className="size-4 animate-spin" /> {children}
    </div>
  );
}

/**
 * What to show when there is no connection at all.
 *
 * Two runtimes reach this and they are not in the same situation. The desktop
 * holds only the hosts it was told about, and the one inside it may not have
 * started — something went wrong. A **hub** has no bootstrap connection at all
 * (`hub-console.md`), so a hub nobody has added a host to yet is simply new.
 * `firstHostCopy` is what keeps a first run from reading as a failure.
 *
 * The ordinary browser build still cannot reach it: its bootstrap connection
 * exists whether or not the host answers, and an unreachable one is a *console*
 * rendering an error rather than an absence.
 *
 * The host switcher stays on screen above this (see `ConsoleChrome`), and the
 * button below opens its dialog. Both, deliberately: the switcher is where an
 * operator will look next time, and the button is the answer *this* time —
 * telling somebody with nothing on screen to go and find a control is not an
 * answer, it is a description of one.
 */
function NoConnection({ starting, desktop }: { starting: boolean; desktop: boolean }) {
  const { setAddingHost } = useHosts();
  const copy = firstHostCopy(desktop);
  return (
    <FullScreen>
      {starting ? (
        <Waiting>
          <span data-testid="no-connection-starting">Starting the host on this computer…</span>
        </Waiting>
      ) : (
        <div className="max-w-sm space-y-3" data-testid="no-connection">
          <p className="text-sm font-medium">{copy.title}</p>
          <p className="text-sm text-muted-foreground">{copy.body}</p>
          <Button data-testid="no-connection-add" onClick={() => setAddingHost(true)}>
            {copy.action}
          </Button>
        </div>
      )}
    </FullScreen>
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

/**
 * What to tell someone whose magic link did not redeem.
 *
 * The counterpart of {@link hubNotice}, and it exists for the same reason: a
 * refused sign-in that says nothing renders the ordinary form, which is
 * indistinguishable from the screen a cold visit gets. A link that lapsed after
 * fifteen minutes is the *routine* outcome of clicking one out of a mailbox the
 * next morning — not an edge case — and the person who does it has no reason to
 * believe pressing "Email me a link" will behave any differently (#1305).
 *
 * Every line is about the *credential* or the *host*: "expired", "already
 * used", "couldn't reach". None of them names an address or admits that one has
 * an account here, so this leaks exactly as little as the silence it replaces —
 * which is the rule the whole sign-in surface is built around, and the reason
 * the host answers `invalid_login` to an unknown address, a lapsed code and a
 * spent one alike. That single answer is also why the first case below has to
 * name both causes: the console genuinely cannot tell which one happened.
 */
export function magicLinkNotice(err: unknown): string {
  const api = err instanceof ApiError ? err : null;
  // A host that cannot be reached checked nothing, so the link may well still
  // be good. Saying it expired would send someone off to request a second one
  // that cannot arrive either.
  if (api?.status === 0) {
    return "We couldn't reach this company's host, so that sign-in link wasn't checked. Try again in a moment.";
  }
  switch (api?.code) {
    case "invalid_login":
      return "That sign-in link didn't work — links expire after 15 minutes and can only be used once. Request a new one below.";
    case "auth_mode":
      // The company changed how it signs people in while the link sat in a
      // mailbox. "Request a new one" would be advice for a form that is no
      // longer on screen, so this one points at whatever replaced it.
      return "This company doesn't sign in with email links any more. Use the sign-in shown below.";
    default:
      return "That sign-in link didn't work. Request a new one below.";
  }
}
