// Picking a transport for the environment the console happens to be running in.

import { BrowserTransport } from "./browser";
import { ProxyTransport } from "./proxy";
import type { Transport } from "./types";

export type {
  StreamHandlers,
  Transport,
  TransportRequest,
  TransportResponse,
} from "./types";
export { BrowserTransport } from "./browser";
export { ProxyTransport } from "./proxy";

/**
 * Whether this console is running inside a Tauri webview with the bridge the
 * desktop transport reads.
 *
 * A **runtime** check, not a build flag, and deliberately so: `frontend/` stays
 * one build artifact that the Rust host can serve from
 * `OPENCOMPANY_CONSOLE_DIR` and the desktop can load from disk. Two builds
 * would mean the desktop-only paths stop being typechecked by the web build,
 * which is exactly how a shared codebase quietly stops being shared.
 *
 * Probes `__TAURI__` — the global that `app.withGlobalTauri` in `tauri.conf.json`
 * injects — because that is the very object `tauriCore()` reads. Tauri v2 always
 * injects the lower-level `__TAURI_INTERNALS__`, but only the `withGlobalTauri`
 * global carries `core.invoke` and `core.Channel`; a probe and a reader that
 * looked at different globals would select the desktop transport in an
 * environment where it cannot bridge. Also probed defensively because this runs
 * under Vitest's `node` environment, where `window` does not exist at all.
 *
 * Presence, not shape — `tauriCore()` is what knows the shape, and the split is
 * deliberate. A `__TAURI__` whose `core.invoke` does not resolve is a broken
 * desktop rather than a browser, and `ProxyTransport` saying so beats
 * `BrowserTransport` quietly failing CORS against every host
 * ([#616](https://github.com/tinyhumansai/opencompany/issues/616)).
 */
export function isDesktopRuntime(): boolean {
  return typeof window !== "undefined" && "__TAURI__" in window;
}

/**
 * Whether a base url names a host this runtime can actually reach.
 *
 * A browser can reach anything it is given, including the empty string, which
 * means *same origin*: `opencompany serve` mounts the console at the origin
 * serving these assets, so `BrowserTransport` resolves `/api/v1/…` against a
 * document origin that answers. Nothing about that path changes here.
 *
 * The desktop is the runtime with a rule, and the rule is **absolute or
 * nothing**. `ProxyTransport` hands the base url to Rust, which joins it to a
 * path by concatenation — so anything without an authority yields a *relative*
 * url, and `reqwest` refuses those at `send()`. The request never reaches a
 * socket, and the console says "could not be reached" about a host it never
 * addressed. That is issue #613, whose reported form was the empty
 * string — the shortest relative url, and only the shortest. `"/api"` and
 * `"localhost:8080"` fail identically, and the second is what a person types
 * into "Add a host".
 *
 * The protocol check is not belt and braces: `URL` parses `tauri://localhost`
 * and `file:///x` without complaint, and neither is a company host.
 *
 * A runtime check rather than a build flag, and beside `defaultTransport`
 * because the two are one decision — what a base url means depends on which
 * transport is going to carry it.
 */
export function isAddressableBaseUrl(baseUrl: string): boolean {
  if (!isDesktopRuntime()) return true;
  try {
    const { protocol } = new URL(baseUrl);
    return protocol === "http:" || protocol === "https:";
  } catch {
    return false;
  }
}

/**
 * Whether this client must carry its own session to `baseUrl`, instead of
 * letting the browser attach a cookie.
 *
 * **Derived from the address, never configured.** "Can a cookie work here"
 * already has an exact answer — the host sets its session cookie
 * `SameSite=Lax`, so it rides same-origin requests and is withheld from every
 * cross-site one — and a flag restating that would be a second source of truth
 * free to disagree with the first. When it disagreed the symptom would be a
 * console that signs in successfully and is then treated as anonymous by every
 * request after it, which reads as a server bug.
 *
 * So: same origin means the cookie, and the cookie stays the better credential
 * wherever it is available, because nothing in the page can read it. A
 * different origin means no cookie is coming and the console has to hold a
 * token itself — which is what a hub console is built out of.
 *
 * True on the desktop too, for every cross-origin address, and the reason is
 * one rung below the browser's: the core's HTTP client has no cookie jar —
 * `reqwest` without the `cookies` feature — so the cookie a sign-in would
 * otherwise set is discarded on arrival (issue #1855). Until this said so, the
 * desktop signed in on the cookie path and was anonymous from the next request
 * onward, which is the exact symptom the paragraph above warns about.
 *
 * What differs on the desktop is not whether the session is carried but WHO
 * holds it: `adoptSession` hands it to the core, which stores it in the OS
 * keychain and attaches it proxy-side — so the token still never lives in the
 * page, which is the property this function's old desktop exemption was
 * guarding. That exemption guarded it by making sign-in impossible; the core
 * holding the token guards it while letting sign-in work.
 */
export function needsCarriedSession(baseUrl: string): boolean {
  // The empty string *is* same-origin, by the convention `ConsoleConfig` sets.
  if (baseUrl === "") return false;
  if (typeof window === "undefined") return false;
  try {
    return new URL(baseUrl, window.location.href).origin !== window.location.origin;
  } catch {
    // An unparseable address reaches nothing, so it needs no credential.
    // Reporting that is `isAddressableBaseUrl`'s job, not this function's.
    return false;
  }
}

/**
 * Whether a secret may be sent to `baseUrl`.
 *
 * **HTTPS, or a host that is this machine.** A device session is a person's
 * standing authority on a company, and plain HTTP puts it in front of every
 * device on the path — a LAN, a café, an office switch. Reading an unencrypted
 * host anonymously is not this; the exposure is entirely in the credential, so
 * that is what this gates and nothing else (issue #731).
 *
 * Loopback is exempt because `http://127.0.0.1:<port>` is how the embedded host
 * is reached and a certificate for an ephemeral port cannot exist. `localhost`
 * comes with it — RFC 6761 reserves the name, and it is what a developer types.
 *
 * ## This is a second copy of a rule that lives in Rust
 *
 * Deliberately, and the duplication is the point rather than a smell:
 * `may_carry_a_credential` in `src-tauri/src/proxy/mod.rs` is the one that
 * enforces it, because a check up here is bypassed by anything reaching the
 * proxy directly. This copy exists so the console can say *why* before it asks
 * — the core's refusal arrives as an opaque IPC rejection that
 * `client.ts` flattens into "cannot reach the company host", which is the
 * indistinguishable failure #613 was about.
 *
 * Kept beside {@link isAddressableBaseUrl} because they are the transport's two
 * questions about one url and they answer different things: that one asks
 * whether the host can be reached, this one whether it can be trusted with a
 * secret. Neither implies the other, and collapsing them would either forbid
 * anonymous HTTP or permit credentialed HTTP.
 */
export function mayCarryACredential(baseUrl: string): boolean {
  let url: URL;
  try {
    url = new URL(baseUrl);
  } catch {
    return false;
  }
  if (url.protocol === "https:") return true;
  // `URL.hostname` brackets nothing and lowercases the host, but an IPv6
  // literal keeps the brackets it was written with.
  const host = url.hostname.replace(/^\[|\]$/g, "");
  return isLoopbackHost(host);
}

/**
 * Whether a host names the machine the console is running on.
 *
 * Matched as text because the platform offers no IP type. That is safe here
 * only because `URL` has already normalised what it was given: `127.1` and
 * `0177.0.0.1` both arrive as `127.0.0.1`, so the shorthand spellings cannot
 * walk past a literal comparison.
 *
 * `127.0.0.0/8` in full rather than `127.0.0.1` alone — the whole block is
 * loopback, and a second local host on `127.0.0.2` is an ordinary thing to do.
 */
function isLoopbackHost(host: string): boolean {
  if (host === "localhost" || host.endsWith(".localhost")) return true;
  // The v6 loopback, and the v4-mapped form — which `URL` rewrites to hex, so
  // `::ffff:127.0.0.1` is not the string that reaches here.
  if (host === "::1" || host === "::ffff:7f00:1") return true;
  const v4 = /^(\d{1,3})\.(\d{1,3})\.(\d{1,3})\.(\d{1,3})$/.exec(host);
  if (!v4) return false;
  const octets = v4.slice(1).map(Number);
  return octets.every((o) => o <= 255) && octets[0] === 127;
}

/**
 * The transport for this environment.
 *
 * Selected here so no caller has to learn the difference. A `connectionId` is
 * required for the desktop lane — the Rust core addresses hosts by id and has
 * no notion of a current one — so a call without it stays on the browser
 * transport, which is correct for the one place that has no connection yet
 * (the bootstrap probe).
 */
export function defaultTransport(connectionId?: string): Transport {
  if (connectionId && isDesktopRuntime()) return new ProxyTransport(connectionId);
  return new BrowserTransport();
}
