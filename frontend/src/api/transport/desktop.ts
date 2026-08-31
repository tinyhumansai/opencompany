// Telling the Rust core which hosts this console talks to.
//
// `ProxyTransport` addresses a host by connection id, and the core resolves
// that id against `ProxyRegistry`. Nothing in the console registered anything,
// so every proxied request came back `no such connection: <id>` — the desktop
// could not complete one round trip. This module is the missing half.
//
// ## Registration is awaited, not fired and forgotten
//
// `addConnection` is synchronous (React renders off it) and `oc_connect` is
// not. Kicking the command off and hoping it lands first is a race the console
// loses on a fast probe, and the symptom — an unreachable host that becomes
// reachable on retry — reads like a network fault rather than an ordering bug.
//
// So each registration is kept as a promise and `ProxyTransport` awaits it
// before its first call. After that the promise is already resolved and the
// await costs a microtask.
//
// ## No implicit current connection
//
// Every entry point here takes an explicit id, for the same reason the Rust
// side does: a single "active connection" is what stops comparable clients
// from holding more than one host, and a convenience default here would
// reintroduce it above the seam instead of below it.

import type { SshTarget } from "@/connections/types";

import { tauriCore } from "./bridge";

/** What `oc_embedded` answers with. Mirrors `EmbeddedInfo` in Rust. */
export interface EmbeddedInfo {
  baseUrl: string;
  dataDir: string;
  /**
   * Who is listening at `baseUrl`, as opposed to where.
   *
   * Optional here though the core always sends it, because this is the one
   * field a *stale* build of the shell would omit: the console is bundled into
   * the binary, but a developer running `pnpm dev` against an older `cargo`
   * build is an ordinary Tuesday. Absent degrades to the pre-identity
   * behaviour rather than to a connection keyed on `undefined`.
   */
  instanceId?: string;
}

/**
 * How a connection authenticates, in the shape `oc_connect` takes.
 *
 * Only the platform bearer travels, because that one genuinely arrives in the
 * URL. A session the desktop holds is the client's own business — it lives in
 * the OS keychain and the console never sees it, which is also why there is no
 * device token here to leak.
 */
export interface DesktopCredential {
  platformToken?: string;
}

/** Connections the core has been told about, by id. */
const registrations = new Map<string, Promise<void>>();

/**
 * Registers a host with the core, replacing any previous registration for `id`.
 *
 * Resolves when the core has the connection. A failure is swallowed into a
 * resolved promise rather than left rejected: the transport awaits this on
 * every call, and an unhandled rejection stored in a module-level map would
 * resurface on each one. The request that follows fails on its own merits with
 * `no such connection`, which is the honest error and the one the console
 * already renders per row.
 */
export function registerConnection(
  id: string,
  baseUrl: string,
  credential: DesktopCredential = {},
): Promise<void> {
  const desktop = tauriCore();
  if (!desktop) return Promise.resolve();

  // Chained onto whatever is already parked under this id, in both directions.
  // `forgetConnection` sequences its disconnect behind a pending registration;
  // without the same courtesy here, a re-register racing an in-flight
  // `oc_disconnect` can land first and then be disconnected by it — the same
  // ordering bug, mirrored. Whichever call came last wins, which is what a
  // caller means by calling it.
  const previous = registrations.get(id) ?? Promise.resolve();
  const pending = previous
    .then(() =>
      desktop.invoke<void>("oc_connect", {
        connectionId: id,
        baseUrl,
        platformToken: credential.platformToken ?? null,
      }),
    )
    .then(
      () => undefined,
      (error: unknown) => {
        console.error(`[desktop] could not register connection ${id}`, error);
      },
    );
  registrations.set(id, pending);
  return pending;
}

/**
 * Hands a sign-in's session to the core, as `id`'s credential (issue #1855).
 *
 * The desktop cannot hold a session the way a hub console does: the proxy
 * strips a webview-supplied `x-opencompany-session` (`RESERVED_HEADERS`) and
 * the event stream never takes caller headers at all — both on purpose, so the
 * page never decides what a request authenticates as. The one place a sign-in's
 * token can live and work is the core, which is where pairing always kept it.
 *
 * Sequenced behind whatever is parked under this id, like `forgetConnection`:
 * an adoption racing an in-flight `oc_connect` would otherwise be overwritten
 * by a registration that read the keychain before the session was in it.
 *
 * The returned promise REJECTS on failure, unlike `registerConnection`'s
 * swallowed one, because this caller is a sign-in with a person behind it — a
 * session that could not be stored must surface where they can retry, not as a
 * mysterious 401 three requests later. The map entry parks the settled-either-
 * way promise so later calls still sequence and nothing resurfaces.
 */
export function adoptSessionIntoCore(id: string, session: string): Promise<void> {
  const desktop = tauriCore();
  if (!desktop) return Promise.resolve();
  const previous = registrations.get(id) ?? Promise.resolve();
  const adopted = previous.then(() =>
    desktop.invoke<void>("oc_adopt_session", { connectionId: id, session }),
  );
  registrations.set(
    id,
    adopted.then(
      () => undefined,
      (error: unknown) => {
        console.error(`[desktop] could not adopt the session for ${id}`, error);
      },
    ),
  );
  return adopted;
}

/**
 * Drops a host from the core.
 *
 * Sequenced after any registration still in flight. This and
 * `registerConnection` race the same way a request does: `oc_connect` resolving
 * *after* `oc_disconnect` landed would leave the connection registered in the
 * core while the console believes it is gone — a dangling entry that the next
 * `registerConnection` for a reused id may or may not overwrite cleanly.
 *
 * Stays synchronous because callers are React event handlers, so the ordering
 * is expressed by chaining onto the pending promise rather than by awaiting it.
 * The map entry is cleared only once the disconnect has been issued; deleting
 * it up front is what dropped the ordering in the first place.
 */
export function forgetConnection(id: string): void {
  const desktop = tauriCore();
  if (!desktop) {
    registrations.delete(id);
    return;
  }
  const pending = registrations.get(id) ?? Promise.resolve();
  const disconnected: Promise<void> = pending
    .then(() => desktop.invoke<void>("oc_disconnect", { connectionId: id }))
    .then(
      () => undefined,
      (error: unknown) => {
        console.error(`[desktop] could not drop connection ${id}`, error);
      },
    )
    .finally(() => {
      // Only remove the entry this call created. A `registerConnection` that
      // landed while the disconnect was in flight owns the id now, and clearing
      // its promise would let a request run before its registration.
      if (registrations.get(id) === disconnected) registrations.delete(id);
    });
  // Parked under the same id so a `connectionReady` in between waits for the
  // disconnect rather than racing past it.
  registrations.set(id, disconnected);
}

/**
 * Resolves once `id` is registered, immediately when there is nothing pending.
 *
 * The "nothing pending" case is not an error: it covers the browser build and
 * any caller that registered before this module was involved.
 */
export function connectionReady(id: string): Promise<void> {
  return registrations.get(id) ?? Promise.resolve();
}

/**
 * The in-process host, when this build has one.
 *
 * `null` in a browser, and also in a desktop whose embedded host failed to
 * start — most often because another instance holds the data root. That is a
 * state the console shows rather than a reason to have no console: the point of
 * the desktop is that it can talk to remote hosts too.
 */
export async function embeddedHost(): Promise<EmbeddedInfo | null> {
  const desktop = tauriCore();
  if (!desktop) return null;
  try {
    return (await desktop.invoke<EmbeddedInfo | null>("oc_embedded")) ?? null;
  } catch (error) {
    console.error("[desktop] could not read the embedded host", error);
    return null;
  }
}

/** One host this machine runs. Mirrors `LocalInstanceInfo` in Rust. */
export interface LocalInstance {
  /** Stable within this machine, and the name of its data directory. */
  id: string;
  /** What the operator called it. Free text, and renameable. */
  label: string;
  dataDir: string;
  running: boolean;
  /** Present exactly when `running` — a stopped instance has no port. */
  baseUrl?: string;
  /**
   * The host's own durable identity, which is what a connection row is keyed
   * on. The address is not: it is a fresh ephemeral port every launch.
   */
  instanceId?: string;
  companies?: string[];
  /** Why it is not running. Usually another process holding its data root. */
  error?: string;
}

/**
 * Every host this machine runs, listening or not.
 *
 * `[]` in a browser, which runs none. Also `[]` on a shell built before the
 * roster existed: the command is simply absent there, and `App` falls back to
 * {@link embeddedHost} — one instance is what that shell has anyway, so the
 * degrade is exact rather than approximate.
 */
export async function localInstances(): Promise<LocalInstance[] | null> {
  const desktop = tauriCore();
  if (!desktop) return [];
  try {
    const answer = await desktop.invoke<LocalInstance[]>("oc_local_instances");
    // Anything that is not an array is a shell that does not implement this.
    // Checked rather than defaulted to `[]`: an unknown command answers
    // `undefined` on some bridges and rejects on others, and both mean the same
    // thing — ask `oc_embedded` instead.
    return Array.isArray(answer) ? answer : null;
  } catch (error) {
    // `null`, not `[]`: "this shell has no roster command" and "this machine
    // runs nothing" are different answers, and only the first has a fallback.
    console.warn("[desktop] this shell has no instance roster", error);
    return null;
  }
}

/**
 * Adds a host on this machine over a data root of its own, and starts it.
 *
 * A root of its own is the whole mechanism: two hosts over one root overwrite
 * each other's companies, which is why the core locks it. So a second local
 * company is a second root, not a second process.
 */
export async function createLocalInstance(label: string): Promise<LocalInstance> {
  const desktop = tauriCore();
  if (!desktop) throw new Error("running a host locally needs the desktop application");
  return desktop.invoke<LocalInstance>("oc_create_local_instance", { label });
}

export async function startLocalInstance(id: string): Promise<LocalInstance> {
  const desktop = tauriCore();
  if (!desktop) throw new Error("running a host locally needs the desktop application");
  return desktop.invoke<LocalInstance>("oc_start_local_instance", { id });
}

/**
 * Stops a host, freeing its port and its data root.
 *
 * Freeing the root is the part worth wanting: it is what lets an
 * `opencompany serve` in a terminal take over the same company.
 */
export async function stopLocalInstance(id: string): Promise<LocalInstance> {
  const desktop = tauriCore();
  if (!desktop) throw new Error("running a host locally needs the desktop application");
  return desktop.invoke<LocalInstance>("oc_stop_local_instance", { id });
}

export async function renameLocalInstance(id: string, label: string): Promise<LocalInstance> {
  const desktop = tauriCore();
  if (!desktop) throw new Error("running a host locally needs the desktop application");
  return desktop.invoke<LocalInstance>("oc_rename_local_instance", { id, label });
}

/**
 * Drops a host from the roster. **The data stays on disk** — the core does the
 * reversible half only, because the other half is someone's company.
 */
export async function forgetLocalInstance(id: string): Promise<void> {
  const desktop = tauriCore();
  if (!desktop) throw new Error("running a host locally needs the desktop application");
  await desktop.invoke<void>("oc_forget_local_instance", { id });
}

/** Permanently deletes a desktop-created host and everything in its data root. */
export async function deleteLocalInstance(id: string): Promise<void> {
  const desktop = tauriCore();
  if (!desktop) throw new Error("deleting a local company needs the desktop application");
  await desktop.invoke<void>("oc_delete_local_instance", { id });
}

/**
 * One tunnel this application is holding open. Mirrors `SshTunnelInfo` in Rust.
 */
export interface SshTunnel {
  /** Stable for a target across launches, and what closing one names. */
  id: string;
  destination: string;
  remotePort: number;
  /** The loopback address to address this host at, this launch. */
  baseUrl: string;
  /** Why it stopped forwarding, in `ssh`'s own words. */
  error?: string;
}

/**
 * Opens a tunnel to a host on another machine, and answers with the address to
 * use for it.
 *
 * Idempotent per target: a host already tunnelled answers with the tunnel that
 * is up. That is what lets the probe call this unconditionally rather than the
 * console keeping its own idea of which tunnels exist.
 *
 * Rejects with what `ssh` printed — "Host key verification failed" and
 * "Permission denied (publickey)" being the two likely ones, both of which the
 * operator has to go and fix in a specific way that only `ssh` knows.
 */
export async function openSshTunnel(target: SshTarget): Promise<SshTunnel> {
  const desktop = tauriCore();
  if (!desktop) throw new Error("reaching a host over ssh needs the desktop application");
  return desktop.invoke<SshTunnel>("oc_open_ssh_tunnel", { target });
}

/**
 * Closes the tunnel to a target.
 *
 * Named by the target rather than by the id {@link SshTunnel} carries, so the
 * roster key stays derived on the core's side alone. A connection restored
 * from `localStorage` has the target and has never seen the id, and a second
 * copy of that derivation here would be a rule two languages have to keep in
 * step.
 *
 * Not an error when there is no such tunnel — removal can arrive twice.
 */
export async function closeSshTunnel(target: SshTarget): Promise<void> {
  await tauriCore()?.invoke<void>("oc_close_ssh_tunnel", { target });
}

/**
 * Every tunnel, and which of them stopped forwarding.
 *
 * `[]` in a browser, which holds none. `null` on a shell built before tunnels
 * existed — the same distinction {@link localInstances} draws, and for the same
 * reason: "this shell cannot do it" and "there are none" have different
 * answers.
 */
export async function sshTunnels(): Promise<SshTunnel[] | null> {
  const desktop = tauriCore();
  if (!desktop) return [];
  try {
    const answer = await desktop.invoke<SshTunnel[]>("oc_ssh_tunnels");
    return Array.isArray(answer) ? answer : null;
  } catch (error) {
    console.warn("[desktop] this shell has no ssh tunnels", error);
    return null;
  }
}

/**
 * Whether a coding harness can be used right now, and if not, what to do
 * about it. Mirrors `Readiness` in Rust (`acp::discovery`) — a tagged union
 * over `state`, exactly as `serde`'s `tag = "state"` emits it.
 */
export type AcpReadiness =
  | { state: "notInstalled" }
  /**
   * The CLI is installed here; the ACP adapter that fronts it is not.
   *
   * Distinct from `notInstalled` because the fix is different and the wrong
   * one is actively misleading: this operator has Claude Code, uses it, and
   * needs one npm package — not a reinstall of software they already run.
   */
  | { state: "adapterMissing"; cli: string; package: string }
  /**
   * No `node` on the shell PATH.
   *
   * Distinct from every other failure because installing an adapter would not
   * help: both adapters are `#!/usr/bin/env node` scripts, so a missing runtime
   * defeats a perfectly good install. Rendering an Install button here offers
   * an action that cannot work.
   */
  | { state: "nodeMissing" }
  /** This app's own adapter is behind the version this build pins. */
  | { state: "adapterOutdated"; found: string; want: string }
  | { state: "notSignedIn" }
  /**
   * Installed and signed in as far as the filesystem can tell, but not yet
   * confirmed to start. A *pending* answer, not a verdict — resolve it with
   * {@link confirmAcpHarness}. Never render this as usable.
   */
  | { state: "checking" }
  | { state: "ready" }
  | { state: "spawnFailed"; reason: string };

/** One coding harness this shell can drive over ACP. Mirrors `HarnessStatus`. */
export interface AcpHarnessStatus {
  id: string;
  label: string;
  readiness: AcpReadiness;
}

/**
 * Every coding harness this shell knows how to drive over ACP, and whether
 * each is ready.
 *
 * `[]` in a browser, which has no local harnesses to speak of. `null` on a
 * shell built before this command existed — same distinction
 * {@link localInstances} draws, and for the same reason: "no command" and "no
 * harnesses installed" are different answers, and only the settings panel
 * that reads this needs to tell them apart.
 */
export async function acpHarnesses(): Promise<AcpHarnessStatus[] | null> {
  const desktop = tauriCore();
  if (!desktop) return [];
  try {
    const answer = await desktop.invoke<AcpHarnessStatus[]>("oc_acp_harnesses");
    return Array.isArray(answer) ? answer : null;
  } catch (error) {
    console.warn("[desktop] this shell has no ACP harness catalogue", error);
    return null;
  }
}

/** One model a harness advertises. Mirrors `HarnessModel`. */
export interface AcpHarnessModel {
  /** The id to send back when pinning a teammate. Must round-trip exactly. */
  value: string;
  /** A human label when the adapter gives one, else render `value`. */
  name?: string;
  description?: string;
  /** Whether the adapter reports this as what it would use right now. */
  current: boolean;
}

/** What a confirmation found: whether it runs, and what it can run. */
export interface AcpConfirmation {
  readiness: AcpReadiness;
  /** Empty when the adapter advertises no choosable model. */
  models: AcpHarnessModel[];
  /**
   * Where the adapter turned out to be — present only on the states that quote
   * it, and resolved after the verdict rather than before it.
   *
   * The survey no longer carries a path, because it no longer looks: nothing
   * is known about a harness until it has been started.
   */
  path?: string;
}

/**
 * Every harness confirmed this session, keyed by id.
 *
 * Deliberately module-level rather than component state: Settings probes on
 * open and the agent detail page needs the same answer, and without a shared
 * cache each would spawn the CLI again for a list that cannot have changed in
 * between.
 *
 * Never persisted. The list is a fact about the installed CLI at this moment —
 * `codex-acp` gained three models between one capture and the next — so a
 * stored copy would go quietly stale and offer models the harness no longer
 * has.
 */
const confirmations = new Map<string, AcpConfirmation>();

/**
 * Starts one harness, resolving its `checking` state to `ready` or
 * `spawnFailed`, and reporting the models it advertises.
 *
 * Separate call from {@link acpHarnesses} on purpose: the list paints from the
 * cheap filesystem probe straight away and each row settles on its own, so one
 * slow CLI cannot hold up the rest of the pane.
 *
 * `null` when nothing can answer — a browser, or a shell predating the
 * command. Callers should leave the row on `checking` in that case rather than
 * inventing a verdict.
 */
export async function confirmAcpHarness(id: string): Promise<AcpConfirmation | null> {
  const desktop = tauriCore();
  if (!desktop) return null;
  try {
    const answer = await desktop.invoke<AcpConfirmation>("oc_acp_confirm_harness", { id });
    if (!answer || typeof answer.readiness?.state !== "string") return null;
    const confirmation: AcpConfirmation = {
      readiness: answer.readiness,
      models: Array.isArray(answer.models) ? answer.models : [],
      path: answer.path,
    };
    confirmations.set(id, confirmation);
    return confirmation;
  } catch (error) {
    console.warn(`[desktop] could not confirm the \`${id}\` harness`, error);
    return null;
  }
}

/**
 * The models `id` advertised when it was last confirmed, or `[]` if it has not
 * been — which is not the same as "it has none", so a caller rendering a
 * picker should treat empty as "nothing to offer yet" and fall back to free
 * text rather than to an empty dropdown.
 */
export function cachedAcpModels(id: string): AcpHarnessModel[] {
  return confirmations.get(id)?.models ?? [];
}

/**
 * Confirms `id` unless it already has been this session.
 *
 * What a surface calls when it wants the model list but does not care about
 * readiness — the agent detail page, which is reached without ever opening
 * Settings.
 */
export async function ensureAcpModels(id: string): Promise<AcpHarnessModel[]> {
  const known = confirmations.get(id);
  if (known) return known.models;
  return (await confirmAcpHarness(id))?.models ?? [];
}

/**
 * Installs (or updates) the ACP adapter this app owns for `id`.
 *
 * Resolves to `null` on success, or the reason it failed — npm's own words
 * where there are any, because a message this layer invented would be a guess
 * about a failure it did not diagnose (a yanked version, a proxy, a read-only
 * home all fail differently and are fixed differently).
 *
 * Evicts the cached confirmation for `id` before returning. That cache exists
 * so the agent model picker does not re-spawn a CLI for a list that cannot
 * have changed — but an install is precisely the event that changes it, and a
 * stale entry would keep serving the pre-install answer for the rest of the
 * session, including the `adapterMissing` that prompted the install.
 */
export async function installAcpHarness(id: string): Promise<string | null> {
  const desktop = tauriCore();
  if (!desktop) return "Installing a coding harness needs the desktop app.";
  try {
    await desktop.invoke<void>("oc_acp_install_harness", { id });
    confirmations.delete(id);
    return null;
  } catch (error) {
    // Cleared on failure too: a partial install leaves the previous answer
    // no more trustworthy than a successful one would.
    confirmations.delete(id);
    return error instanceof Error ? error.message : String(error);
  }
}

/** Test seam: forget every registration. */
export function resetDesktopRegistrations(): void {
  registrations.clear();
  // The confirmation cache is module-level, so a test that left one behind
  // would leak a harness's model list into the next test's expectations.
  confirmations.clear();
}
