// @vitest-environment jsdom
//
// Issue #1855: a desktop sign-in to a remote host must produce a credential
// that actually works.
//
// The failure this pins was silent end to end. The desktop signed in on the
// cookie path (`needsCarriedSession` said the desktop never carries a session),
// the host answered with an `HttpOnly` cookie, and the Rust core — whose HTTP
// client has no cookie jar — discarded it. Every request after a successful
// sign-in was anonymous, the resulting 401 was masked to "down" by the
// prosumer-alias fallback, and a `cloud` row then spun for the whole wake
// window over a host that had answered, precisely, "sign in".
//
// Three rules, each of which failed quietly:
//
//   1. a cross-origin address needs a carried session in EVERY runtime — the
//      desktop most of all, because it has no cookie jar anywhere;
//   2. on the desktop the session goes to the CORE, never into the page —
//      the proxy strips a webview-supplied session header by design, so a
//      page-held token authenticates nothing;
//   3. a 401 from the companies list is an answer, and no fallback's 404 may
//      outrank it into "down".

import { afterEach, beforeEach, describe, expect, it } from "vitest";

import { needsCarriedSession } from "@/api/transport";
import type { StreamHandlers, Transport, TransportRequest, TransportResponse } from "@/api/transport";
import {
  addConnection,
  adoptSession,
  getConnection,
  probe,
  resetConnections,
} from "@/connections/registry";
import { connectionConfig } from "@/connections/types";

/** The v2 shape `isDesktopRuntime()` and `tauriCore()` read. */
function desktop(present: boolean, invoke?: (cmd: string, args: unknown) => Promise<unknown>) {
  if (present) {
    (window as unknown as { __TAURI__: unknown }).__TAURI__ = {
      core: { invoke: invoke ?? (() => Promise.resolve()), Channel: class {} },
    };
  } else {
    delete (window as unknown as { __TAURI__?: unknown }).__TAURI__;
  }
}

/** Answers each path with what the test staged; 404 for anything unstaged. */
class RouteTransport implements Transport {
  constructor(private readonly routes: Record<string, { status: number; text: string }>) {}
  async request(req: TransportRequest): Promise<TransportResponse> {
    const path = new URL(req.url).pathname;
    const staged = this.routes[path] ?? { status: 404, text: '{"error":"not found"}' };
    return {
      status: staged.status,
      statusText: "",
      url: req.url,
      text: staged.text,
      header: () => null,
    };
  }
  subscribe(_url: string, _handlers: StreamHandlers): () => void {
    return () => {};
  }
}

beforeEach(() => {
  window.localStorage.clear();
  resetConnections();
});

afterEach(() => {
  desktop(false);
  resetConnections();
});

describe("a cross-origin address needs a carried session in every runtime (#1855)", () => {
  it("says so on the desktop, where the old exemption made sign-in impossible", () => {
    desktop(true);
    // jsdom's origin is localhost; a remote host is another origin entirely.
    expect(needsCarriedSession("https://smoke1.staging.example.com")).toBe(true);
  });

  it("still lets a same-origin console keep its cookie", () => {
    desktop(true);
    // The empty string IS same-origin, by `ConsoleConfig`'s own convention —
    // and the cookie stays the better credential wherever it works, because
    // nothing in the page can read it.
    expect(needsCarriedSession("")).toBe(false);
  });

  it("is unchanged in the browser", () => {
    desktop(false);
    expect(needsCarriedSession("https://smoke1.staging.example.com")).toBe(true);
    expect(needsCarriedSession("")).toBe(false);
  });
});

describe("adopting a sign-in on the desktop (#1855)", () => {
  it("hands the session to the core and keeps the token out of the page", async () => {
    const invokes: Array<{ cmd: string; args: unknown }> = [];
    desktop(true, (cmd, args) => {
      invokes.push({ cmd, args });
      return Promise.resolve();
    });
    const id = addConnection({
      baseUrl: "https://acme.example.com",
      transport: new RouteTransport({}),
    });

    await adoptSession(id, "acme.a-session-token");

    // The core got it, addressed to this connection.
    const adopted = invokes.find((i) => i.cmd === "oc_adopt_session");
    expect(adopted?.args).toMatchObject({ connectionId: id, session: "acme.a-session-token" });

    // The page did not keep it: the credential records only WHERE the session
    // lives, and the client this connection builds carries no session header —
    // the proxy would strip one anyway, which is the whole point.
    const connection = getConnection(id);
    expect(connection?.credential.kind).toBe("device");
    expect(connectionConfig(connection!).sessionHeader).toBeNull();
    expect(JSON.stringify(connection)).not.toContain("a-session-token");
  });

  it("leaves the credential untouched when the core refuses the session", async () => {
    // The core refuses for a reason it names — a locked keychain, a plain-HTTP
    // remote host. Recording a `device` credential anyway would have every
    // check treat this connection as signed-in while every request runs
    // anonymous: the silence this whole issue is about, one layer up.
    desktop(true, (cmd) =>
      cmd === "oc_adopt_session"
        ? Promise.reject(new Error("this host is not encrypted, so a credential cannot be sent to it"))
        : Promise.resolve(),
    );
    const id = addConnection({
      baseUrl: "https://acme.example.com",
      transport: new RouteTransport({}),
    });

    await expect(adoptSession(id, "acme.a-session-token")).rejects.toThrow("not encrypted");

    const connection = getConnection(id);
    expect(connection?.credential.kind).not.toBe("device");
    expect(JSON.stringify(connection)).not.toContain("a-session-token");
  });

  it("keeps the browser path exactly as it was: the console holds the token", async () => {
    desktop(false);
    const id = addConnection({
      baseUrl: "https://acme.example.com",
      transport: new RouteTransport({}),
    });

    await adoptSession(id, "acme.a-session-token");

    const connection = getConnection(id);
    expect(connection?.credential.kind).toBe("session");
    expect(connectionConfig(connection!).sessionHeader).toBe("acme.a-session-token");
  });
});

describe("a refused credential reads as a sign-in, not an outage (#1855)", () => {
  it("lets the companies list's 401 outrank the prosumer alias's 404", async () => {
    // A platform host that wants a sign-in: the list 401s, and the alias —
    // which exists for single-company hosts — answers the 404 it answers
    // everyone. Before the fix `statusFromError(statusErr ?? listErr)` let
    // that 404 win, reported "down", and a cloud row then retried "down" for
    // the whole 90s wake window: a spinner over a sign-in screen.
    const id = addConnection({
      baseUrl: "https://tenant.example.com",
      transport: new RouteTransport({
        "/spec": { status: 200, text: "{}" },
        "/api/v1/companies": { status: 401, text: '{"error":"sign in"}' },
        "/api/v1/company": { status: 404, text: '{"error":"not found"}' },
      }),
    });

    await probe(id);

    expect(getConnection(id)?.status).toBe("unauthenticated");
  });

  it("still reports a genuinely absent host as down", async () => {
    // The fallback's own answer keeps mattering when the list's failure was
    // not an auth refusal — a host with neither surface is still gone.
    const id = addConnection({
      baseUrl: "https://tenant.example.com",
      transport: new RouteTransport({
        "/spec": { status: 200, text: "{}" },
        "/api/v1/companies": { status: 500, text: '{"error":"broken"}' },
        "/api/v1/company": { status: 404, text: '{"error":"not found"}' },
      }),
    });

    await probe(id);

    expect(getConnection(id)?.status).toBe("down");
  });
});
