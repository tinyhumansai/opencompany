// The set of hosts this console is talking to, and their live status.
//
// A module-level store read through React 18's `useSyncExternalStore`. No state
// library, deliberately: the console has none today, and the absence of a
// global query cache is the single biggest thing protecting this refactor. A
// cache keyed on anything less than (connection, company) is exactly how two
// hosts' data gets mixed, and the way to not have that bug is to not have the
// cache.
//
// Note what is *not* here: an "active connection". Selecting a connection in
// the UI is a rendering choice, not a state change in this module — every
// connection stays probed and, later, streamed. A single-valued active
// connection is precisely what stops buzz from holding more than one relay, and
// reintroducing it here would undo the whole slice.

import { useSyncExternalStore } from "react";

import { OpenCompanyClient } from "@/api/client";
import { ApiError } from "@/api/types";
import {
  defaultTransport,
  isAddressableBaseUrl,
  isDesktopRuntime,
  mayCarryACredential,
} from "@/api/transport";
import type { Transport } from "@/api/transport";
import {
  closeSshTunnel,
  forgetConnection,
  openSshTunnel,
  adoptSessionIntoCore,
  registerConnection,
} from "@/api/transport/desktop";
import {
  EMBEDDED_LABEL,
  type ConnectionProfile,
  localProfiles,
  connectorOf,
  findProfile,
  findSshProfile,
  forgetProfile,
  readProfiles,
  saveProfile,
} from "./profileStore";
import {
  type Connection,
  type ConnectionId,
  type Connector,
  type Credential,
  type InstanceIdentity,
  DEFAULT_CONNECTOR,
  connectionConfig,
} from "./types";
import { keepWaking, wakeRetryDelay } from "./waking";

/** The alphabet a generated connection id uses. Excludes `:` — see `scopedKey`. */
const ID_ALPHABET = "abcdefghijklmnopqrstuvwxyz0123456789";

function mintId(): ConnectionId {
  let out = "";
  const bytes = new Uint8Array(12);
  if (typeof crypto !== "undefined" && "getRandomValues" in crypto) {
    crypto.getRandomValues(bytes);
  } else {
    // Node, under the unit tests. Uniqueness within one process is all that is
    // needed there; this never runs in a browser.
    for (let i = 0; i < bytes.length; i += 1) bytes[i] = Math.floor(Math.random() * 256);
  }
  for (const byte of bytes) out += ID_ALPHABET[byte % ID_ALPHABET.length];
  return out;
}

interface Entry {
  connection: Connection;
  client: OpenCompanyClient;
  /**
   * Kept beside the client because the client holds it privately, and
   * `adoptCredential` has to rebuild the client over the *same* transport — a
   * connection that silently fell back to `fetch` mid-session would bypass the
   * desktop's proxy and its CORS-free lane with it.
   */
  transport: Transport;
}

let entries: Entry[] = [];
let listeners: Array<() => void> = [];
/**
 * The snapshot handed to React.
 *
 * Cached and only replaced when something actually changes, because
 * `useSyncExternalStore` compares snapshots by identity and would loop forever
 * on a fresh array every call.
 */
let snapshot: Connection[] = [];
/**
 * Connections with a probe in flight.
 *
 * Without this, probing is self-perpetuating: `probe` sets the status to
 * `connecting`, that emits a new snapshot, and any effect watching the
 * connection list and looking for `connecting` fires again — probing forever
 * and taking the tab down with it. Making the guard a property of the registry
 * rather than of the caller means every future caller inherits it.
 *
 * Probes are additionally versioned by the address they are working against:
 * `reseat` swaps in a new client when a connection moves, so a probe that
 * captured the previous address must discard its writes (`stale`) and the
 * `probe` finalizer starts the replacement client's probe it suppressed.
 */
const probing = new Set<ConnectionId>();

function emit(): void {
  snapshot = entries.map((e) => e.connection);
  for (const listener of listeners) listener();
}

function subscribe(listener: () => void): () => void {
  listeners.push(listener);
  return () => {
    listeners = listeners.filter((l) => l !== listener);
  };
}

function getSnapshot(): Connection[] {
  return snapshot;
}

function patch(id: ConnectionId, change: Partial<Connection>): void {
  let touched = false;
  entries = entries.map((entry) => {
    if (entry.connection.id !== id) return entry;
    touched = true;
    return { ...entry, connection: { ...entry.connection, ...change } };
  });
  if (touched) emit();
}

/**
 * True when a probe working against `address` is describing a connection that
 * has since moved on.
 *
 * `reseat` swaps in a new `OpenCompanyClient` (built from a new address) for
 * the same id, so a probe that captured the previous one would otherwise patch
 * identity, companies, and status fetched from the *old* address over the new
 * connection's row. Discarding by address rather than by client identity lets
 * a label-only `reseat` keep its in-flight probe's writes, which are still
 * describing the same host.
 */
function stale(id: ConnectionId, address: string | undefined): boolean {
  return clientFor(id)?.baseUrl !== address;
}

/** Every connection, for rendering. */
export function useConnections(): Connection[] {
  return useSyncExternalStore(subscribe, getSnapshot, getSnapshot);
}

/** One connection, or `undefined` once it has been removed. */
export function useConnection(id: ConnectionId | null): Connection | undefined {
  const all = useConnections();
  return id === null ? undefined : all.find((c) => c.id === id);
}

export function listConnections(): Connection[] {
  return snapshot;
}

export function getConnection(id: ConnectionId): Connection | undefined {
  return entries.find((e) => e.connection.id === id)?.connection;
}

/**
 * The client for a connection.
 *
 * One per connection and reused, so that `useEffect` dependencies keyed on the
 * client do not re-fire on every render — and so a connection's `onUnauthorized`
 * hook belongs to that connection alone.
 */
export function clientFor(id: ConnectionId): OpenCompanyClient | undefined {
  return entries.find((e) => e.connection.id === id)?.client;
}

export interface AddConnection {
  baseUrl: string;
  label?: string;
  /** The company the client addresses by default; `null` for the alias form. */
  defaultCompany?: string | null;
  credential?: Credential;
  /** Injected in tests, and by the desktop shell. */
  transport?: Transport;
  /**
   * What is already known about the host, before `/spec` is asked.
   *
   * Only the embedded host has any: this client starts it, so the core can
   * hand over its identity without a round trip. A probe replaces this with
   * the host's own fuller answer.
   */
  identity?: InstanceIdentity;
  /** Where this host runs. Defaults to `remote` — a url someone supplied. */
  connector?: Connector;
}

/**
 * Registers a host, reusing its remembered id when there is one.
 *
 * **Reuse is not a nicety.** Every browser-local key is scoped by the
 * connection id, so a freshly minted id on each page load would orphan the tour
 * state, the last-read channel and the mail draft on every reload — with no
 * error anywhere. `findProfile` is what makes the id stable for a host across
 * reloads; see `profileStore.ts`.
 *
 * Does not contact the host — call {@link probe}.
 */
export function addConnection(input: AddConnection): ConnectionId {
  const baseUrl = input.baseUrl.replace(/\/$/, "");
  const defaultCompany = input.defaultCompany ?? null;
  // An `ssh` host is recognised by where its tunnel goes, not by the loopback
  // port this launch happened to bind — see `findSshProfile`. Every other
  // connector's address is what it is remembered by.
  const remembered =
    input.connector?.kind === "ssh"
      ? findSshProfile(input.connector.target)
      : findProfile(baseUrl, defaultCompany);

  // Already registered this session (StrictMode double-invokes, and the web
  // build adds its bootstrap connection from a `useMemo`). Hand back the
  // existing entry rather than a duplicate row for one host.
  if (remembered) {
    const existing = entries.find((e) => e.connection.id === remembered.id);
    if (existing) {
      // A caller that brought a credential wins over whatever this entry was
      // restored with. `restoreConnections` runs first and can only supply what
      // was written down — which is never a platform bearer, by design (see
      // `profileStore.persistable`). The bootstrap add that follows carries the
      // live one from `?token=`, and returning early without taking it would
      // leave the connection permanently unauthenticated on every reload.
      if (input.credential) adoptCredential(existing.connection.id, input.credential);
      return existing.connection.id;
    }
  }

  const id = remembered?.id ?? mintId();
  const connection: Connection = {
    id,
    label: input.label ?? rememberedLabel(remembered) ?? hostLabel(baseUrl),
    baseUrl,
    defaultCompany,
    credential: input.credential ?? { kind: "cookie" },
    status: "connecting",
    identity: input.identity ?? null,
    companies: [],
    connector: input.connector ?? DEFAULT_CONNECTOR,
  };
  // The desktop routes this connection through its own core; the browser
  // build keeps `fetch`. `defaultTransport` decides, so neither the registry
  // nor the client has to know which shell it is in.
  const transport = input.transport ?? defaultTransport(id);
  const client = new OpenCompanyClient(connectionConfig(connection), transport);
  // Per connection, so one host refusing this client's credential marks that
  // row and leaves the other N-1 alone. The globally-fatal version of this is
  // what made a single expired session blank the whole console.
  client.onUnauthorized = () => patch(id, { status: "unauthenticated" });
  entries = [...entries, { connection, client, transport }];
  announceToDesktop(connection);
  saveProfile(profileOf(connection));
  emit();
  return id;
}

/**
 * What of a connection outlives the session.
 *
 * One function rather than a literal at each call site, because every field
 * omitted at one of them is a field silently dropped on the next write —
 * `origin` in particular, whose whole job is to survive.
 */
function profileOf(connection: Connection): ConnectionProfile {
  return {
    id: connection.id,
    baseUrl: connection.baseUrl,
    label: connection.label,
    defaultCompany: connection.defaultCompany,
    credential: connection.credential,
    instanceId: connection.identity?.instanceId,
    connector: connection.connector,
    // Written beside it for one release so a rolled-back build still
    // recognises a local host; see `ConnectionProfile.origin`.
    origin: connection.connector.kind === "local" ? "embedded" : undefined,
  };
}

/**
 * Registers every host remembered from a previous session.
 *
 * Without this the profile store would only ever *stabilise the id* of a
 * connection something else re-added — so the bootstrap host would come back on
 * reload and every host the operator added by hand would quietly not. Restoring
 * is what makes "connected to N hosts" a property of the client rather than of
 * one page load.
 *
 * Idempotent: `addConnection` returns the existing entry for a host already
 * registered, so calling this alongside the bootstrap add cannot double up.
 */
/**
 * Re-points an already-registered connection at a different credential.
 *
 * The client bakes the credential in at construction (`connectionConfig` reads
 * it into `operatorToken`), so this replaces the client rather than mutating
 * one — a half-updated client that kept the old bearer for requests already
 * configured would be worse than either state.
 */
function adoptCredential(id: ConnectionId, credential: Credential): void {
  const existing = entries.find((e) => e.connection.id === id);
  if (!existing) return;
  if (sameCredential(existing.connection.credential, credential)) return;
  reseat(id, { ...existing.connection, credential });
}

/**
 * Replaces a connection's record and the client built from it.
 *
 * The client reads `baseUrl` and the credential into its config at
 * construction, so anything the config is derived from is replaced rather than
 * mutated: a half-updated client that kept the old address for requests
 * already configured would be worse than either state. The core is re-told for
 * the same reason — it resolves proxied requests against its own copy.
 */
function reseat(id: ConnectionId, connection: Connection): void {
  const existing = entries.find((e) => e.connection.id === id);
  if (!existing) return;
  const client = new OpenCompanyClient(connectionConfig(connection), existing.transport);
  client.onUnauthorized = () => patch(id, { status: "unauthenticated" });
  entries = entries.map((e) =>
    e.connection.id === id ? { connection, client, transport: existing.transport } : e,
  );
  announceToDesktop(connection);
  saveProfile(profileOf(connection));
  emit();
}

function sameCredential(a: Credential, b: Credential): boolean {
  if (a.kind !== b.kind) return false;
  // A restored profile carries `{ kind: "platform" }` with no token, so this
  // comparison is what makes the bootstrap's live token count as a change.
  if (a.kind === "platform" && b.kind === "platform") return a.token === b.token;
  if (a.kind === "device" && b.kind === "device") return a.ref === b.ref;
  // Signing in again mints a *new* session, so the values differ and the client
  // has to be rebuilt around the new one. Comparing only the kind here would
  // leave the console presenting the session it just replaced — which keeps
  // working until the old one expires or is revoked, making it the kind of bug
  // that surfaces an hour later and nowhere near its cause.
  if (a.kind === "session" && b.kind === "session") return a.value === b.value;
  return true;
}

/**
 * The same-origin console this page load *is*.
 *
 * Passed by `App` when the bootstrap host is the origin serving the bundle,
 * which is every ordinary web deployment. Absent in the desktop, where a
 * same-origin profile is unreachable and dropped outright by
 * {@link keepIfReachable}, and in a console pointed elsewhere with `?api=`,
 * which makes no claim about what lives at its own origin.
 */
export interface SameOriginConsole {
  /** What `?company=` names on this load; `null` for the alias form. */
  defaultCompany: string | null;
}

export function restoreConnections(
  transport?: Transport,
  sameOrigin?: SameOriginConsole,
): ConnectionId[] {
  return readProfiles()
    .filter(keepIfReachable)
    .filter((profile) => isThisConsole(profile, sameOrigin))
    .map((profile) =>
      addConnection({
        baseUrl: profile.baseUrl,
        label: rememberedLabel(profile),
        defaultCompany: profile.defaultCompany,
        credential: profile.credential,
        identity: profile.instanceId ? { instanceId: profile.instanceId } : undefined,
        connector: connectorOf(profile),
        transport,
      }),
    );
}

/**
 * Drops a stored profile this runtime could never reach, and forgets it.
 *
 * There is exactly one such profile in practice: the same-origin entry a
 * desktop build wrote before it stopped adding one (issue #613). Not adding it
 * any more is not enough on its own — this store is what brings a connection
 * back, so the dead row would be restored on every launch forever, and it sorts
 * ahead of the embedded host because it was written first.
 *
 * Forgotten rather than merely skipped: a row the console refuses to restore is
 * a row nothing will ever remove, and `oc.connections.v1` is a registry people
 * read when a host misbehaves. It should say what the console holds.
 */
function keepIfReachable(profile: ConnectionProfile): boolean {
  if (isAddressableBaseUrl(profile.baseUrl)) return true;
  forgetProfile(profile.id);
  return false;
}

/**
 * Whether a same-origin profile is *this* page load's console, or a past one.
 *
 * The second half of issue #1167, and the reason the duplicate row existed at
 * all. Profiles are keyed on `(baseUrl, defaultCompany)` — deliberately, so
 * `?company=a` and `?company=b` keep their view state apart (`findProfile`) —
 * but the empty base url is one host, whichever company a link named. So every
 * distinct `?company=` ever opened against this origin left a durable profile
 * behind, and this function is what used to bring all of them back: one row per
 * company ever visited, all at the same address, all with the same name, and
 * nothing that ever expired them.
 *
 * They are not extra hosts. A connection already carries every company its host
 * serves, and `switchCompany` moves between them *inside* one console
 * (`ConnectionConsole.tsx`) — so a second row for the same origin offers an
 * operator a choice that changes nothing but which of two identical rows is
 * ticked.
 *
 * Skipped rather than forgotten, which is the distinction `retireConnection`
 * exists for: the profile stays, so opening `?company=b` again lands on the
 * same connection id and the same scoped state (`scopedKey`) rather than a
 * freshly minted one.
 */
function isThisConsole(
  profile: ConnectionProfile,
  sameOrigin: SameOriginConsole | undefined,
): boolean {
  if (sameOrigin === undefined) return true;
  if (profile.baseUrl !== "") return true;
  return profile.defaultCompany === sameOrigin.defaultCompany;
}

/** Where a host running inside this application is, and who it is. */
export interface EmbeddedHostInfo {
  baseUrl: string;
  /** Absent only on a shell predating `instance_id` on `oc_embedded`. */
  instanceId?: string;
  /**
   * What to call it, when the core has a name from the operator.
   *
   * Absent for the host at the data root, which keeps {@link EMBEDDED_LABEL} —
   * and absent from a shell predating the roster, which has only that one.
   */
  label?: string;
  /** Injected in tests. */
  transport?: Transport;
}

/**
 * Registers the host running inside this application, of which there is one.
 *
 * Not `addConnection`, and the difference is the whole fix (#615). The embedded
 * host binds an ephemeral port on purpose — a fixed one collides with a dev
 * server — so its address is different on every launch, while `addConnection`
 * recognises a host *by* its address. Each launch therefore looked like a first
 * meeting: a new id, a new row, and last launch's row left behind pointing at a
 * closed port. They are durable, so they accumulated, and they carry the same
 * label as the live one — leaving an operator a sidebar of identical entries,
 * all but one broken, with nothing to tell them apart.
 *
 * So this matches on identity instead, and enforces the invariant the type
 * states: at most one embedded connection exists at a time.
 *
 * Reusing the remembered id rather than minting a fresh one is what carries the
 * tour state, the last-read channel and the mail draft across a relaunch — all
 * of them keyed by connection id (see `scopedKey`).
 */
export function adoptEmbeddedHost(host: EmbeddedHostInfo): ConnectionId {
  return adoptLocalHosts([host])[0];
}

/**
 * Registers every host running inside this application, and drops the rest.
 *
 * The generalisation of {@link adoptEmbeddedHost} to a machine running more
 * than one instance, and the reason the pruning could not stay where it was:
 * the single-host version treated *any other* embedded profile as last
 * launch's dead row and removed it. With a roster, another embedded profile is
 * ordinarily the operator's second company — so the set has to be pruned
 * against the set, not against one member of it.
 *
 * Kept as one call rather than a loop of {@link adoptEmbeddedHost} for exactly
 * that reason: the prune needs to see every live instance before it removes
 * anything, and N calls each see one.
 *
 * A **stopped** instance is not passed as a host — it has no address, so a
 * connection for it could only sit in the rail failing its probe. But its id
 * must still be passed in `knownInstanceIds`, and the difference is the whole
 * of the second bug this signature exists to prevent.
 *
 * `removeConnection` forgets the *persisted profile*, and a connection id is
 * what every browser-local key is scoped by (see `scopedKey`). So pruning a
 * stopped instance as though it were a ghost means its tour state, last-read
 * channel and mail draft are orphaned the moment it is stopped, and it comes
 * back from a restart wearing a freshly minted id — #615 again, reached by
 * pressing Stop instead of by relaunching.
 *
 * A stopped instance therefore has its live entry retired and its profile
 * kept: no row in the rail, no lost namespace. Only a profile matching *no*
 * instance the core knows about is a genuine ghost, and only that is forgotten.
 */
export function adoptLocalHosts(
  hosts: EmbeddedHostInfo[],
  /**
   * Every instance the core holds, running or not.
   *
   * Defaulted to the running set so a caller that passes one argument keeps
   * the old meaning — but `App` always passes the whole roster, because the
   * running set alone cannot tell "stopped" from "gone".
   */
  knownInstanceIds: readonly string[] = hosts
    .map((host) => host.instanceId)
    .filter((id): id is string => id !== undefined),
): ConnectionId[] {
  const known = localProfiles();
  const rostered = new Set(knownInstanceIds);
  // Matched first, all of them, before anything is removed. `thisHost` falls
  // back to an id-less profile — the one an older version wrote — and two
  // hosts must not both adopt it, or two connections share one namespace.
  const claimed = new Set<ConnectionId>();
  const mine = hosts.map((host) => {
    const match = thisHost(
      known.filter((profile) => !claimed.has(profile.id)),
      host.instanceId,
    );
    if (match) claimed.add(match.id);
    return match;
  });

  for (const profile of known) {
    if (claimed.has(profile.id)) continue;
    if (profile.instanceId !== undefined && rostered.has(profile.instanceId)) {
      // A stopped instance. Its entry goes — nothing is listening, and a row
      // that cannot answer is the dead row this whole function exists to
      // prevent — but its profile stays, so starting it again lands on the
      // same connection id and the same scoped state.
      retireConnection(profile.id);
      continue;
    }
    // A genuine ghost: a previous launch's address, or a data root this
    // application no longer serves. Nothing is listening there and nothing
    // ever will be again, so it is dropped rather than left failing its probe
    // in the rail forever.
    removeConnection(profile.id);
  }

  return hosts.map((host, index) => adoptOne(host, mine[index]));
}

function adoptOne(
  host: EmbeddedHostInfo,
  mine: ConnectionProfile | undefined,
): ConnectionId {
  const baseUrl = host.baseUrl.replace(/\/$/, "");
  const identity = host.instanceId ? { instanceId: host.instanceId } : undefined;

  if (mine) {
    const registered = entries.find((e) => e.connection.id === mine.id);
    if (registered) {
      // `restoreConnections` already put it back at last launch's address.
      return reseatEmbedded(registered.connection, baseUrl, identity, host.label);
    }
    // Not registered this session. Write the new address down first, so the
    // `addConnection` below finds this profile by it and reuses the id.
    saveProfile({
      ...mine,
      baseUrl,
      label: host.label ?? mine.label,
      instanceId: host.instanceId ?? mine.instanceId,
    });
  }

  return addConnection({
    baseUrl,
    // The core's name wins over the remembered one: renaming an instance
    // happens there, and a stale label in `localStorage` would quietly outrank
    // it forever.
    label: host.label ?? mine?.label ?? EMBEDDED_LABEL,
    identity,
    connector: { kind: "local" },
    transport: host.transport,
  });
}

/**
 * Which remembered profile, if any, is the instance now running.
 *
 * Identity decides when both ends know it: a *different* id at this address is
 * a different host — a second data root, say — and adopting its row would merge
 * two hosts' local state, which is the failure `types.ts` exists to prevent.
 *
 * A profile with no id recorded is one an older version wrote, before the core
 * reported one. There is nothing to compare, and this application had exactly
 * one embedded host then too, so it is adopted rather than orphaned.
 */
function thisHost(
  known: ConnectionProfile[],
  instanceId: string | undefined,
): ConnectionProfile | undefined {
  const byIdentity =
    instanceId === undefined
      ? undefined
      : known.find((p) => p.instanceId === instanceId);
  return byIdentity ?? known.find((p) => p.instanceId === undefined);
}

/** Moves a registered embedded connection to the address it is now serving. */
function reseatEmbedded(
  connection: Connection,
  baseUrl: string,
  identity: InstanceIdentity | undefined,
  label?: string,
): ConnectionId {
  if (
    connection.baseUrl === baseUrl &&
    connection.connector.kind === "local" &&
    (label === undefined || connection.label === label)
  ) {
    // The same host at the same address: a second call in one session, which
    // StrictMode guarantees. Re-seating would throw away a probe in flight.
    return connection.id;
  }
  reseat(connection.id, {
    ...connection,
    baseUrl,
    label: label ?? connection.label,
    connector: { kind: "local" },
    identity: identity ?? connection.identity,
    // Whatever the last probe concluded, it concluded about the old address.
    status: "connecting",
    error: undefined,
  });
  return connection.id;
}

/**
 * Tells the desktop core about a connection, so its proxy can address it.
 *
 * A no-op in a browser. Registration is awaited by `ProxyTransport` rather than
 * here, because `addConnection` is synchronous — React renders off its return
 * value — and blocking it on an IPC round trip would stall the first paint on
 * every host.
 *
 * A `device` credential is not forwarded: its `ref` names a keychain entry and
 * nothing resolves one yet, so passing the handle through as if it were a token
 * would authenticate as the literal string "keychain-handle". Such a connection
 * registers unauthenticated and the host answers 401, which the row already
 * renders.
 */
function announceToDesktop(connection: Connection): void {
  if (!isDesktopRuntime()) return;
  void registerConnection(
    connection.id,
    connection.baseUrl,
    connection.credential.kind === "platform" && connection.credential.token
      ? { platformToken: connection.credential.token }
      : {},
  );
}

/**
 * Records that this machine now holds a paired device session for `id`.
 *
 * The `ref` is the device id the host minted — a name for the pairing, useful
 * when someone is looking at the host's device list deciding what to revoke. It
 * is emphatically not the credential: the core resolves the session from the
 * keychain by connection id, and this record only says that one exists.
 */
export function pairedConnection(id: ConnectionId, deviceId: string): void {
  adoptCredential(id, { kind: "device", ref: deviceId });
}

/** Forgets a pairing locally. The host's session record is untouched. */
export function unpairConnection(id: ConnectionId): void {
  adoptCredential(id, { kind: "cookie" });
}

/**
 * Records that the host has revoked this connection's session.
 *
 * Call only once the host has answered the revocation, never optimistically.
 * Drops the client-held credential too: a `session` or `device` connection
 * carries its own token, which would otherwise be re-presented on the next
 * load.
 */
export function signedOut(id: ConnectionId): void {
  adoptCredential(id, { kind: "cookie" });
  patch(id, { status: "unauthenticated" });
}

/**
 * Records the session a cross-origin sign-in just returned.
 *
 * Called with the `session` from a {@link SignIn}, and it must be called or the
 * sign-in is wasted: the host returns that token exactly once, keeps only its
 * hash, and — because no cookie was set — the console has no other way to prove
 * who it is. The symptom of forgetting is a login that appears to succeed and a
 * console that is anonymous from the next request onward.
 *
 * Replaces the client, through `adoptCredential`, so every request after this
 * carries the new session rather than the one it was constructed with.
 */
export async function adoptSession(id: ConnectionId, session: string): Promise<void> {
  // On the desktop the session goes to the CORE, not into this client
  // (issue #1855). The webview cannot use it: the proxy strips a caller-
  // supplied `x-opencompany-session` (`RESERVED_HEADERS`) and the event
  // stream carries only the core-held credential — both deliberate, so the
  // page never decides what a request authenticates as. The core stores it in
  // the keychain under this connection id, which is exactly where a paired
  // device's session lived and what `oc_connect` reads back on the next
  // launch — so a sign-in here survives an app restart, which the browser's
  // never could.
  //
  // Awaited by the caller before it probes: a probe that ran first would
  // authenticate with the pre-sign-in credential and conclude the host still
  // refuses us.
  if (isDesktopRuntime()) {
    await adoptSessionIntoCore(id, session);
    // `device` rather than a webview-held `session`: the record says where the
    // credential lives, and every check keyed off it — the insecure-transport
    // refusal, what a probe may claim — already treats a core-held credential
    // correctly under this kind. The ref names the pairing for a person; a
    // sign-in has no device id, so it says what it is.
    adoptCredential(id, { kind: "device", ref: "signed-in" });
    return;
  }
  adoptCredential(id, { kind: "session", value: session });
}

/**
 * Points an explicit-company connection at a different company (issue #1807).
 *
 * `defaultCompany` is what `ConnectionConsole`'s boot effect reads on every
 * mount — a `?company=` link or a single-company profile takes the "explicit
 * company wins" path straight to `client.status(defaultCompany)` rather than
 * listing. A reset provisions the replacement into the *current* session
 * directly (`switchCompany`), which leaves this untouched: the next reload
 * would still ask for the just-archived id, `client.status` would answer
 * `company_not_found`, and the operator would land on a connection error
 * instead of back in their own console.
 *
 * Re-seats like {@link editConnection}, so the rebuilt client and the
 * persisted profile agree with the roster immediately, not just after the
 * next probe.
 *
 * A no-op for a connection that was never company-scoped (`defaultCompany`
 * already `null`) — a multi-company connection has nothing to retarget, and
 * forcing one narrows it to a single company it was never addressed as.
 */
export function retargetDefaultCompany(id: ConnectionId, company: string): void {
  const existing = getConnection(id);
  if (!existing || existing.defaultCompany === null || existing.defaultCompany === company) {
    return;
  }
  reseat(id, { ...existing, defaultCompany: company });
}

/**
 * Clears an explicit-company connection's persisted default (issue #1807).
 *
 * For the abandon path: a reset's archive leg landed, but nothing replaced
 * it — the operator cancelled, or gave up retrying a failed create. There is
 * no replacement id to retarget to, so unlike {@link retargetDefaultCompany}
 * this drops the scoping entirely rather than moving it, sending the next
 * boot down the multi-company/picker path instead of retrying an id that no
 * longer exists.
 *
 * A no-op for a connection that was never company-scoped, same as
 * {@link retargetDefaultCompany}.
 */
export function clearDefaultCompany(id: ConnectionId): void {
  const existing = getConnection(id);
  if (!existing || existing.defaultCompany === null) return;
  reseat(id, { ...existing, defaultCompany: null });
}

/**
 * Rewrites the `?company=` URL param to `newId` in place, or clears it, to
 * keep the next reload from re-booting a connection into `archivedId`
 * (issue #1807).
 *
 * {@link retargetDefaultCompany} fixes the persisted profile, but
 * `resolveConfig()` re-derives its `company` fresh on every load from THREE
 * sources, in ascending priority — `VITE_OC_COMPANY`, `window.OPENCOMPANY
 * _CONFIG`, then `?company=` (see `config.ts`) — and a reset never touches
 * any of them. Left stale, the *next* reload's bootstrap `addConnection`
 * call looks up `findProfile(baseUrl, archivedId)`, which no longer matches
 * the retargeted profile (its `defaultCompany` has moved), mints a fresh,
 * duplicate connection scoped to the archived id instead of reusing it, and
 * that connection's boot effect asks the host for an id that no longer
 * exists.
 *
 * Only the query layer can be rewritten at runtime — `VITE_OC_COMPANY` is
 * baked in at build time and `window.OPENCOMPANY_CONFIG` is injected once in
 * `index.html`, so neither this function nor anything else client-side can
 * touch them directly. But the query layer outranks both in `resolveConfig`'s
 * merge, so writing an override there works regardless of which of the three
 * the connection's explicit company actually came from — including the
 * config/env case the original fix (query-only) missed (codex review on
 * #1828, PR comment 3864885209).
 *
 * `newId: null` clears the override instead of retargeting to a replacement
 * — the abandon path (a reset that archived but never created), where there
 * is no replacement id to name. An empty `?company=` still outranks the
 * env/window layers on the next `resolveConfig()`, and resolves to `""`,
 * which the boot effect's `if (defaultCompany)` treats as "no explicit
 * company" (falsy), same as `null` (codex review on #1828, PR comment
 * 3864885215).
 *
 * A no-op when the URL already names some OTHER company — that param was
 * never going to resolve to `archivedId` on reload regardless of anything
 * this function does (the query layer already outranks env/window), so
 * overwriting it would clobber an unrelated link rather than fix this one.
 *
 * That guard alone does NOT cover an absent `?company=` param — `current ===
 * null` also passes it, so this still WRITES one in for a connection that
 * never sourced its default company from the URL at all. Whether that write
 * is correct depends on identity, not on what the param currently holds: it
 * is only correct for the one connection `resolveConfig()` produced for the
 * page currently loaded (`ConnectionConsole`'s `isBootstrap` prop, App's
 * `bootstrapId === connectionId`). A restored, non-bootstrap profile can
 * carry its own non-null `defaultCompany` the same way and hit this
 * function via the same call sites, and an absent param there means the
 * *bootstrap* connection's own link (or no link at all) — writing into it
 * anyway points the address bar at a company the connection whose config it
 * actually describes never asked for, and the next reload's
 * `resolveConfig()`/`findProfile` pair mints a duplicate scoped to the wrong
 * host (issue #1828 comment 3865563560). Callers gate on that identity
 * before calling; this function's own guard only ever protected the
 * narrower "URL already names something else" case.
 */
export function retargetCompanyUrlParam(archivedId: string, newId: string | null): void {
  if (typeof window === "undefined") return;
  const url = new URL(window.location.href);
  const current = url.searchParams.get("company");
  if (current !== null && current !== archivedId) return;
  url.searchParams.set("company", newId ?? "");
  window.history.replaceState(null, "", `${url.pathname}${url.search}${url.hash}`);
}

/** What "modify a host" may change about one. See {@link editConnection}. */
export interface HostEdit {
  /** What this connection is called in the switcher. Blank keeps the old name. */
  label?: string;
  /** Where the host is. Only honoured where {@link hostAddressEditable} is true. */
  baseUrl?: string;
}

/**
 * Whether an operator may retype a connection's address.
 *
 * `local` and `ssh` addresses are **assigned by this application**, not typed:
 * a local host binds an ephemeral port on purpose, and a tunnel's address is a
 * loopback port this client chose when it opened it. Both are different on
 * every launch, so an address edited here would be overwritten by the next one
 * — and in the meantime it would point the console at a port nothing is
 * serving. Their names stay editable; their addresses belong to whatever is
 * managing the process.
 */
export function hostAddressEditable(connector: Connector): boolean {
  return connector.kind === "remote" || connector.kind === "cloud";
}

/**
 * Renames a connection, or points it at a different address.
 *
 * Re-seats rather than patches, because the client bakes `baseUrl` into its
 * config at construction — see {@link reseat} — and the desktop core keeps its
 * own copy to resolve proxied requests against. A patched connection would
 * render the new address beside a client still talking to the old one.
 *
 * A move re-probes, and drops everything the last probe concluded: the
 * identity, the company list and the error all describe the host that *was* at
 * the old address, and leaving them in place is how a console ends up naming
 * one host while addressing another.
 */
export function editConnection(id: ConnectionId, change: HostEdit): void {
  const existing = getConnection(id);
  if (!existing) return;
  const label = change.label?.trim() || existing.label;
  const baseUrl =
    change.baseUrl !== undefined && hostAddressEditable(existing.connector)
      ? change.baseUrl.trim().replace(/\/$/, "")
      : existing.baseUrl;
  const moved = baseUrl !== existing.baseUrl;
  if (!moved && label === existing.label) return;
  reseat(id, {
    ...existing,
    label,
    baseUrl,
    ...(moved
      ? {
          status: "connecting" as const,
          error: undefined,
          identity: null,
          companies: [],
          waking: false,
        }
      : {}),
  });
  if (moved) void probe(id);
}

export function removeConnection(id: ConnectionId): void {
  const going = getConnection(id);
  entries = entries.filter((e) => e.connection.id !== id);
  forgetProfile(id);
  if (isDesktopRuntime()) {
    forgetConnection(id);
    // A tunnel outliving the connection it was opened for is an `ssh` process
    // nothing lists and nobody can stop from the application.
    if (going?.connector.kind === "ssh") void closeSshTunnel(going.connector.target);
  }
  emit();
}

/**
 * Drops a connection from this session **without forgetting who it was.**
 *
 * The difference from {@link removeConnection} is one line — `forgetProfile` —
 * and it is the difference between "this host is gone" and "this host is not
 * running right now". A stopped local instance is the second: there is nothing
 * to talk to, so it must not hold a row, but it still owns a connection id
 * that every `scopedKey` under it is named after.
 */
function retireConnection(id: ConnectionId): void {
  const present = entries.some((e) => e.connection.id === id);
  if (!present) return;
  entries = entries.filter((e) => e.connection.id !== id);
  if (isDesktopRuntime()) forgetConnection(id);
  emit();
}

/** Drops every connection. Tests only — the app never empties the registry. */
export function resetConnections(): void {
  entries = [];
  probing.clear();
  emit();
}

/**
 * Contacts a host and records what it found.
 *
 * This is `App`'s old `boot()`, moved here so that discovering the second host
 * is the same code as discovering the first. It resolves rather than throws:
 * a connection that cannot be reached is a *state*, not an exception, because
 * the other connections carry on regardless.
 */
export async function probe(id: ConnectionId): Promise<void> {
  if (!clientFor(id) || probing.has(id)) return;
  probing.add(id);
  patch(id, { status: "connecting", error: undefined, waking: false });
  // The address this probe is working against, so the finalizer below can tell
  // whether the connection moved under it.
  let address: string | undefined = clientFor(id)?.baseUrl;
  try {
    if (!(await ensureTunnel(id))) return;
    const insecure = insecurelyCredentialed(getConnection(id));
    if (insecure) {
      // Refused here rather than left to fail at the core, because this is the
      // one function that owns a row's status — and the core's refusal arrives
      // as an IPC rejection that `client.ts` has already flattened into "cannot
      // reach the company host". Saying it here is what makes the row name the
      // reason instead of blaming a network that is working (issue #731).
      patch(id, { status: "down", error: insecure });
      return;
    }
    // Re-read: `ensureTunnel` may have moved this connection to a new address,
    // which replaces its client. The probe works against the client it finds
    // here; a `reseat` after this point makes its writes stale.
    const client = clientFor(id);
    if (!client) return;
    address = client.baseUrl;
    await probeUntilAwake(id, client, address);
  } finally {
    probing.delete(id);
    // The connection moved while this probe was in flight, so its own
    // `void probe(id)` was suppressed by the in-flight guard and the
    // replacement client never got a probe. Pick it up now that the stale
    // work has cleared.
    const current = clientFor(id);
    if (current && current.baseUrl !== address) void probe(id);
  }
}

/**
 * Makes sure an `ssh` connection has a tunnel under it, and that its address is
 * this launch's rather than last launch's.
 *
 * Every other connector is already reachable or is not, so this answers `true`
 * immediately for them.
 *
 * Done here rather than at startup because the address is the problem. A
 * tunnel binds an ephemeral loopback port, so the url a restored `ssh`
 * connection comes back holding belonged to a tunnel that closed when the
 * application last quit — and probing it would report an unreachable host for
 * a machine that is fine. Opening is idempotent per target on the core's side,
 * so asking on every probe costs one IPC round trip and needs no separate idea
 * of which tunnels are up.
 *
 * A tunnel that cannot be opened is the connection's own failure, carrying
 * `ssh`'s words: "Permission denied (publickey)" is a row an operator can act
 * on, where "could not be reached" is one they cannot.
 */
async function ensureTunnel(id: ConnectionId): Promise<boolean> {
  const connection = getConnection(id);
  if (!connection || connection.connector.kind !== "ssh") return true;
  if (!isDesktopRuntime()) {
    patch(id, { status: "down", error: "reaching a host over ssh needs the desktop application" });
    return false;
  }
  try {
    const tunnel = await openSshTunnel(connection.connector.target);
    if (tunnel.baseUrl !== connection.baseUrl) {
      reseat(id, { ...connection, baseUrl: tunnel.baseUrl, error: undefined });
    }
    return true;
  } catch (err) {
    patch(id, { status: "down", error: err instanceof Error ? err.message : String(err) });
    return false;
  }
}

/**
 * Probes, and keeps probing for as long as the connector says a failure might
 * still be a host on its way up.
 *
 * One attempt for every connector but `cloud`, where the loop is what turns a
 * hibernating tenant into "Waking…" rather than into a red row nothing would
 * ever re-probe — see `waking.ts`. The in-flight guard in {@link probe} is what
 * keeps the loop singular: a second call while this one is waiting returns
 * immediately rather than starting a competing chain.
 */
async function probeUntilAwake(
  id: ConnectionId,
  client: OpenCompanyClient,
  address: string | undefined,
): Promise<void> {
  const startedAt = Date.now();
  for (let attempt = 0; ; attempt += 1) {
    const failure = await runProbe(id, client, address);
    if (!failure) return;
    // Removed or retired while the request was in flight — including by
    // `resetConnections`, which is how a test escapes this loop.
    const connection = getConnection(id);
    if (!connection) return;
    // Moved to a new address while this probe was in flight: its writes would
    // describe a different host, so stop here — the `probe` finalizer starts
    // the replacement client's probe.
    if (stale(id, address)) return;
    const status = failure.status ?? "down";
    if (!keepWaking(connection.connector, status, Date.now() - startedAt)) {
      patch(id, { ...failure, waking: false });
      return;
    }
    patch(id, { status: "connecting", error: undefined, waking: true });
    await sleep(wakeRetryDelay(attempt));
    if (!getConnection(id)) return;
    if (stale(id, address)) return;
  }
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

/**
 * One attempt: patches what it found, and answers with the failure it did not
 * record as final.
 *
 * Returning the failure rather than patching it is what lets the caller decide
 * whether `down` is the end of the story. Success is patched here, because
 * there is nothing to decide about it.
 */
async function runProbe(
  id: ConnectionId,
  client: OpenCompanyClient,
  address: string | undefined,
): Promise<Partial<Connection> | null> {
  // The connection moved while this attempt was in flight: everything below
  // would describe the old address. Answer `null` (the stop signal) so the
  // caller ends without writing; the `probe` finalizer starts the replacement
  // client's probe.
  if (stale(id, address)) return null;
  const identity = await readIdentity(client);
  if (identity && !stale(id, address)) {
    patch(id, { identity, label: identity.displayName ?? labelOf(id) });
  }

  try {
    const companies = await client.listCompanies();
    if (stale(id, address)) return null;
    patch(id, { status: "live", companies: companies.map((c) => c.id), waking: false });
    return null;
  } catch (listErr) {
    // A single-company (prosumer) host has no `/api/v1/companies`; its sole
    // company answers on the alias instead. Falling back rather than failing is
    // what lets one client hold a platform host and a prosumer host at once.
    try {
      await client.status(null);
      if (stale(id, address)) return null;
      patch(id, { status: "live", companies: [], waking: false });
      return null;
    } catch (statusErr) {
      // A 401 from the companies list outranks whatever the alias said
      // (issue #1855). The fallback exists for single-company hosts, and on a
      // platform host it answers 404 for everyone — so letting that 404 win
      // reported "down" about a host that had answered, precisely, "sign in".
      // `keepWaking` then retried `down` for the whole cloud wake window,
      // which is the spinner-over-a-sign-in that `waking.ts` promises never
      // to show.
      if (listErr instanceof ApiError && listErr.status === 401) {
        return statusFromError(listErr);
      }
      return statusFromError(statusErr ?? listErr);
    }
  }
}

/** Reads `/spec`, tolerating a host that has no identity fields yet. */
async function readIdentity(client: OpenCompanyClient): Promise<InstanceIdentity | null> {
  try {
    const spec = await client.get<Record<string, unknown>>("/spec");
    return {
      instanceId: typeof spec.instance_id === "string" ? spec.instance_id : undefined,
      displayName: typeof spec.display_name === "string" ? spec.display_name : undefined,
      // `undefined` is meaningfully different from `[]`: an older host omits
      // the field, and the client must read that as "assume REST only" rather
      // than as "supports nothing".
      capabilities: Array.isArray(spec.capabilities)
        ? (spec.capabilities as string[])
        : undefined,
      storage: typeof spec.storage === "string" ? spec.storage : undefined,
    };
  } catch {
    return null;
  }
}

/**
 * Why this connection must not be contacted, or `null` when it may be.
 *
 * A credential over plain HTTP to a host that is not this machine, which the
 * core refuses at registration (`may_carry_a_credential`). Answering the same
 * question here is what turns that refusal into something a person can act on;
 * see the note on `mayCarryACredential`.
 *
 * Desktop only **for the ambient credentials**. A browser's cookie is one the
 * browser decides about, with `Secure` and the origin's own rules doing this
 * job — narrowing the web build there would refuse the plain-HTTP deployments
 * `opencompany serve` is built for, on the transport where the console is not
 * the thing holding the secret.
 *
 * A `session` credential is the exception, and gated everywhere: a hub console
 * holds that token itself and puts it on the wire itself, so "the browser is
 * deciding" stops being true. The runtime it happens to run in changes nothing
 * about what an unencrypted hop exposes.
 *
 * Read off the profile's credential *kind*, which is a claim about this machine
 * rather than the secret itself: a `device` entry means the keychain holds a
 * session the core will attach, and `platform` with a token means one arrived
 * in the url. `cookie` and a token-less `platform` carry nothing, so a host
 * added by hand and read anonymously stays permitted — the whole point of
 * gating on the credential rather than on the scheme.
 */
function insecurelyCredentialed(connection: Connection | undefined): string | null {
  if (!connection) return null;
  // A same-origin connection is the browser's own origin, whose rules already
  // decide this; there is no url to judge and nothing to protect it from that
  // is not already inside the page.
  if (connection.baseUrl === "") return null;
  // A session is a secret **this page holds and sends**, in every runtime — so
  // unlike the two below it is gated in the browser as well. The exposure is
  // identical to the desktop's: a person's standing authority on a company,
  // travelling in clear text past every device on the path.
  const carriesInAnyRuntime = connection.credential.kind === "session";
  const carriesOnDesktop =
    isDesktopRuntime() &&
    (connection.credential.kind === "device" ||
      (connection.credential.kind === "platform" && Boolean(connection.credential.token)));
  if (!carriesInAnyRuntime && !carriesOnDesktop) return null;
  if (mayCarryACredential(connection.baseUrl)) return null;
  // Names the credential generically: this covers a paired device session and a
  // platform bearer from `?token=`, and the person reading it needs the reason
  // rather than which of the two it was.
  return `${connection.baseUrl} is not encrypted, so this connection's credential cannot be sent to it. Use https, or a host on this machine.`;
}

function statusFromError(err: unknown): Partial<Connection> {
  if (err instanceof ApiError && err.status === 401) {
    return { status: "unauthenticated", error: "this host refused the credential" };
  }
  const message = err instanceof ApiError ? err.message : "could not be reached";
  return { status: "down", error: message };
}

function labelOf(id: ConnectionId): string {
  return getConnection(id)?.label ?? id;
}

/**
 * The name every same-origin connection used to carry, and no longer earns.
 *
 * Kept as a constant because two things still need to recognise it: the
 * fallback below, for a runtime with no address to read, and
 * {@link rememberedLabel}, which has to spot it in a profile written before
 * issue #1167 and re-derive rather than restore it.
 */
export const SAME_ORIGIN_LABEL = "This host";

/**
 * A readable name for a host before it has told us its own.
 *
 * A same-origin host has no url of its own to read, and naming it by a constant
 * is the whole of issue #1167: *every* such connection came out with the same
 * name, so a console holding two offered an operator two identical rows in the
 * host switcher with nothing but the hue of a status dot between them — the
 * failure `adoptLocalHosts` above was written to end, resurfacing somewhere a
 * position and a hover title no longer make up for it.
 *
 * The origin serving this page is what the connection actually addresses, so it
 * is both distinguishing and honest. "This host" is only ever true of one row,
 * and survives here solely for a runtime with no location to read — Node, under
 * the unit tests — where a label is still required and there is nothing better.
 */
export function hostLabel(baseUrl: string): string {
  if (!baseUrl) return sameOriginLabel();
  try {
    return new URL(baseUrl).host;
  } catch {
    return baseUrl;
  }
}

function sameOriginLabel(): string {
  const host = typeof window === "undefined" ? "" : (window.location?.host ?? "");
  return host || SAME_ORIGIN_LABEL;
}

/**
 * The remembered name for a host, or `undefined` when it is not worth keeping.
 *
 * Installs are durable, so dropping the constant from {@link hostLabel} does
 * nothing on its own: every console that has already run wrote "This host" into
 * `oc.connections.v1`, and a remembered label outranks a derived one — so the
 * indistinguishable name would outlive the fix on exactly the machines that
 * reported it. A profile still carrying it is therefore treated as carrying
 * none. Only the same-origin row is treated this way; a host someone typed is
 * named by its address and could never have been given this label.
 */
function rememberedLabel(profile: ConnectionProfile | undefined): string | undefined {
  if (!profile) return undefined;
  if (profile.baseUrl === "" && profile.label === SAME_ORIGIN_LABEL) return undefined;
  return profile.label;
}
