import { expect, test, type Page } from "@playwright/test";



/**
 * What the desktop opens on, driven against a real host.
 *
 * Issue #613: the packaged app added a connection to its own origin — an empty
 * base url, which means "same origin" and is a real host only in a browser —
 * made it the bootstrap, and therefore opened on "Couldn't reach a company host
 * at this origin" every launch while its embedded host sat healthy and
 * unselected in the switcher.
 *
 * The desktop cannot be packaged inside this suite, but the thing that *makes*
 * it a desktop can be: `isDesktopRuntime()` is `"__TAURI__" in window` and
 * nothing else, and every host request then goes through `ProxyTransport` to
 * the bridge stubbed below. So this exercises the real console bundle, the real
 * boot sequence and the real Rust-backed host — with the one seam that defines
 * the desktop standing in for the shell.
 *
 * The stub is deliberately faithful on the point the bug turned on: it refuses
 * a base url that is not absolute, exactly as `ProxyRegistry::upsert` now does,
 * because a stub that quietly resolved `"" + "/api/v1"` would be a stub in
 * which the bug cannot happen.
 */

interface BridgeConfig {
  /** What `oc_embedded` answers. `null` is a desktop whose host did not start. */
  embedded: string | null;
  /**
   * How long the core takes to answer, in milliseconds.
   *
   * Real IPC to a host that has already bound is fast enough that the window
   * between first paint and the embedded host arriving cannot be observed
   * reliably. Widening it deliberately is what makes that window testable —
   * and it is a real window on a cold start, when the host is still binding.
   */
  discoveryDelayMs?: number;
}

/** One `oc_connect` the console made, as the test reads them back. */
interface RegisteredConnection {
  connectionId: string;
  baseUrl: string;
}

declare global {
  interface Window {
    __ocRegistered?: RegisteredConnection[];
    /** Base urls `oc_request` was asked to send to, in order. */
    __ocRequested?: string[];
  }
}

/**
 * Installs a Tauri bridge before the app boots.
 *
 * `oc_request` forwards to `fetch` against the connection's *registered* base
 * url, which is what the Rust proxy does — and, like the proxy, it resolves for
 * every HTTP status rather than throwing, so the console's own error handling
 * is the thing under test.
 */
async function asDesktop(page: Page, config: BridgeConfig) {
  await page.addInitScript((cfg: BridgeConfig) => {
    // The tour modal covers the board and swallows clicks. These are the legacy
    // keys, which every connection adopts on first read — the scoped ones carry
    // a connection id this test cannot know, because the embedded host's is
    // minted at runtime.
    for (const key of ["oc-tour:single", "oc-tour:e2e-harness-co", "oc-tour:null"]) {
      window.localStorage.setItem(key, JSON.stringify({ skipped: true, seenAt: Date.now() }));
    }

    const registered: RegisteredConnection[] = [];
    const requested: string[] = [];
    const hosts = new Map<string, string>();
    window.__ocRegistered = registered;
    window.__ocRequested = requested;

    /** `ProxyRegistry::upsert`'s rule, restated: absolute http(s) or nothing. */
    const isAddressable = (baseUrl: string): boolean => {
      try {
        const url = new URL(baseUrl);
        return (url.protocol === "http:" || url.protocol === "https:") && url.host !== "";
      } catch {
        return false;
      }
    };

    (window as unknown as { __TAURI__: unknown }).__TAURI__ = {
      // Tauri v2 namespaces the API: `withGlobalTauri` injects the whole
      // `@tauri-apps/api` bundle, and `invoke`/`Channel` live under `core`. A shim
      // that puts them at the top level is the v1 shape — the one the console
      // itself used to read (#616) — so a spec built on it would drive a bridge
      // the real app can never resolve.
      core: {
        Channel: class {
          onmessage: ((message: string) => void) | null = null;
        },
        async invoke(command: string, args: Record<string, unknown>): Promise<unknown> {
          switch (command) {
            case "oc_connect": {
              const id = args.connectionId as string;
              const baseUrl = args.baseUrl as string;
              // Recorded before it is judged, and deliberately: this array is the
              // test's window onto what the console *tried* to register. Keeping
              // only what was accepted would let the stub filter out the very row
              // #613 is about, and the assertion would then pass on a build that
              // still adds it.
              registered.push({ connectionId: id, baseUrl });
              // Rejected at registration, where `ProxyRegistry::upsert` rejects
              // it, rather than at the first request. The console swallows this
              // into a resolved promise and the request that follows fails with
              // `no such connection` — which is exactly what the desktop does.
              if (!isAddressable(baseUrl)) {
                throw new Error(`not an absolute host url: "${baseUrl}"`);
              }
              hosts.set(id, baseUrl);
              return undefined;
            }
            case "oc_disconnect": {
              hosts.delete(args.connectionId as string);
              return undefined;
            }
            case "oc_embedded": {
              if (cfg.discoveryDelayMs) {
                await new Promise((resolve) => setTimeout(resolve, cfg.discoveryDelayMs));
              }
              return cfg.embedded === null
                ? null
                : { baseUrl: cfg.embedded, dataDir: "/tmp/e2e-desktop" };
            }
            case "oc_local_instances": {
              // The roster a current console asks first — `App` only falls back to
              // `oc_embedded` when this command is absent, which is not what a
              // packaged shell answers. Mirrors `cfg.embedded`: one running
              // instance when there is a host, none when there is not, so a
              // machine with nothing running is still a machine that can start
              // one — not a shell predating the roster.
              if (cfg.discoveryDelayMs) {
                await new Promise((resolve) => setTimeout(resolve, cfg.discoveryDelayMs));
              }
              return cfg.embedded === null
                ? []
                : [
                    {
                      id: "default",
                      label: "This computer",
                      dataDir: "/tmp/e2e-desktop",
                      running: true,
                      baseUrl: cfg.embedded,
                      instanceId: "default",
                    },
                  ];
            }
            case "oc_request": {
              const id = args.connectionId as string;
              // Only ever a host `oc_connect` accepted, so no second check is
              // needed here — an unaddressable base never reached this map.
              const base = hosts.get(id);
              if (base === undefined) throw new Error(`no such connection: ${id}`);
              // Recorded so a test can assert that a host was never *contacted*,
              // which is a different claim from never being registered — and the
              // one that matters when what must not travel is a credential.
              requested.push(base);
              const req = args.request as {
                method: string;
                path: string;
                headers: Record<string, string>;
                body?: string;
              };
              const response = await fetch(base + req.path, {
                method: req.method,
                headers: req.headers,
                body: req.body ?? undefined,
                credentials: "include",
              });
              const text = await response.text();
              const headers: Record<string, string> = {};
              response.headers.forEach((value, name) => {
                headers[name.toLowerCase()] = value;
              });
              return {
                status: response.status,
                statusText: response.statusText,
                url: response.url,
                text,
                headers,
              };
            }
            default:
              return null;
          }
        },
      },
    };
  }, config);
}

/**
 * A port nothing listens on, so a remembered remote host is reliably down.
 *
 * Being unreachable is what gives the ordering test its teeth: if the console
 * opened on this row rather than on the embedded host, the assertion that the
 * board is on screen could not pass.
 */
const DEAD_REMOTE = "http://127.0.0.1:9";

/** Seeds the dead row a desktop built before this fix wrote on its first run. */
async function seedSameOriginProfile(page: Page) {
  await page.addInitScript(() => {
    window.localStorage.setItem(
      "oc.connections.v1",
      JSON.stringify([
        {
          id: "conn-stale-origin",
          baseUrl: "",
          label: "This host",
          defaultCompany: null,
          credential: { kind: "cookie" },
        },
      ]),
    );
  });
}

test("a desktop opens on its embedded host, not on its own origin", async ({
  page,
  baseURL,
}) => {
  // The embedded host is the host serving this suite: a real OpenCompany at a
  // real absolute address, which is exactly what `oc_embedded` reports on a
  // packaged run.
  await asDesktop(page, { embedded: new URL(baseURL ?? "http://127.0.0.1:8080").origin });
  await seedSameOriginProfile(page);
  await page.goto("/#/ledgers/tasks");

  // THE assertion, and the whole issue: no error panel. Before the fix this
  // read "Couldn't reach a company host at this origin" on every launch.
  await expect(page.getByTestId("connection-error")).toHaveCount(0);
  await expect(page.getByRole("button", { name: "Add task" })).toHaveCount(1, {
    timeout: 30_000,
  });

  // The same-origin connection was never registered with the core. A row that
  // reached `oc_connect` is a row the console believed it could address.
  const registered = await page.evaluate(() => window.__ocRegistered ?? []);
  expect(registered.length).toBeGreaterThan(0);
  expect(registered.map((r) => r.baseUrl)).not.toContain("");

  // And the row a previous build wrote is gone from storage rather than merely
  // skipped — otherwise it returns on the next launch, forever.
  const stored = await page.evaluate(() =>
    JSON.parse(window.localStorage.getItem("oc.connections.v1") ?? "[]"),
  );
  expect(stored.map((p: { baseUrl: string }) => p.baseUrl)).not.toContain("");
});

test("a remembered host does not take the launch just by being older", async ({
  page,
  baseURL,
}) => {
  // The case the fix's *selection* half exists for, and the one a single-host
  // test cannot reach. A host added in some previous session is restored at
  // first paint; the embedded host is appended later, because its port only
  // arrives over IPC. So list order records when each was learned about — and
  // taking the first entry would open the desktop on last Tuesday's remote host
  // instead of on the machine in front of the person. That is #613's shape
  // again, with the dead bootstrap swapped for a stale favourite.
  await asDesktop(page, { embedded: new URL(baseURL ?? "http://127.0.0.1:8123").origin });
  await page.addInitScript((remote: string) => {
    window.localStorage.setItem(
      "oc.connections.v1",
      JSON.stringify([
        {
          id: "conn-remembered-remote",
          baseUrl: remote,
          label: "Remembered remote",
          defaultCompany: null,
          credential: { kind: "cookie" },
        },
      ]),
    );
  }, DEAD_REMOTE);
  await page.goto("/#/ledgers/tasks");

  // Both hosts are registered, so the switcher offers a choice. Counted off the
  // closed trigger, which carries the roster size so a count does not depend on
  // a menu being open.
  await expect(page.getByTestId("host-switcher")).toHaveAttribute("data-host-count", "2", {
    timeout: 30_000,
  });
  // The console on screen is a working one — which it could not be if the
  // unreachable remote had been selected for sorting first.
  await expect(page.getByRole("button", { name: "Add task" })).toHaveCount(1, {
    timeout: 30_000,
  });
  await expect(page.getByTestId("connection-error")).toHaveCount(0);

  // Said off the closed trigger. The per-row `aria-current` this used to read is
  // hidden with the roster (`src/product-scope.ts`), so what survives is the
  // count — the remembered host is still held, not dropped — and the worst
  // status across the set, which is what the rows would have had to agree with.
  // Only the count survives off the closed trigger: the remembered host is still
  // held rather than dropped. Which row is current, and that the current one is
  // live, were per-row facts — the trigger reports the WORST status across the
  // set, which is `down` here precisely because the remembered host is dead, so
  // it cannot stand in for them. The assertion above (no connection error, and
  // the embedded host's console on screen) is what still says the launch went to
  // the right one.
  await expect(page.getByTestId("host-switcher")).toHaveAttribute("data-host-count", /[2-9]/);
});

test("a desktop waits for its own host rather than borrowing a remembered one", async ({
  page,
  baseURL,
}) => {
  // While the core is still answering, "there is no embedded host" and "it has
  // not been asked yet" look identical from the connection list — so falling to
  // the first entry in the meantime opens a remembered host, mounts its console
  // and issues its requests, only to replace it a moment later. Briefly opening
  // the wrong host is the same bug as #613, just shorter.
  await asDesktop(page, {
    embedded: new URL(baseURL ?? "http://127.0.0.1:8123").origin,
    discoveryDelayMs: 1_500,
  });
  await page.addInitScript((remote: string) => {
    window.localStorage.setItem(
      "oc.connections.v1",
      JSON.stringify([
        {
          id: "conn-remembered-remote",
          baseUrl: remote,
          label: "Remembered remote",
          defaultCompany: null,
          credential: { kind: "cookie" },
        },
      ]),
    );
  }, DEAD_REMOTE);
  await page.goto("/#/ledgers/tasks");

  // The startup state, held rather than skipped past. The remembered host is
  // registered by now — it is restored at first paint — so this is a choice not
  // to show it, not an absence of anything to show.
  await expect(page.getByTestId("no-connection-starting")).toBeVisible();
  await expect(page.getByTestId("connection-error")).toHaveCount(0);

  // And when the core does answer, the embedded host is what opens.
  await expect(page.getByRole("button", { name: "Add task" })).toHaveCount(1, {
    timeout: 30_000,
  });
  await expect(page.getByTestId("connection-error")).toHaveCount(0);
});

/**
 * A remote host on plain HTTP, on the network a laptop is actually on.
 *
 * Not loopback, and deliberately unlike `DEAD_REMOTE` above: that one is
 * `127.0.0.1:9`, which this rule *permits* — being unreachable and being
 * unencrypted are different failures, and the point of the test below is that
 * the console now tells them apart.
 */
const INSECURE_REMOTE = "http://192.168.1.20:8080";

test("a paired host on plain http is refused, and says why", async ({ page, baseURL }) => {
  // Issue #731. A device session is a person's standing authority on a company,
  // and `apply_credential` attaches it to every request and to the whole life of
  // the event stream — so a connection remembered against an unencrypted remote
  // address puts it in front of everyone on that network, repeatedly and
  // replayably. The core refuses to register it; this is what the operator sees.
  await asDesktop(page, { embedded: new URL(baseURL ?? "http://127.0.0.1:8123").origin });
  await page.addInitScript((remote: string) => {
    window.localStorage.setItem(
      "oc.connections.v1",
      JSON.stringify([
        {
          id: "conn-paired-cleartext",
          baseUrl: remote,
          label: "Paired over http",
          defaultCompany: null,
          // What a desktop paired before this rule existed wrote down. The ref
          // is a device id, not the secret — the session it names is in the OS
          // keychain, and it is what the core would attach.
          credential: { kind: "device", ref: "dev-1" },
        },
      ]),
    );
  }, INSECURE_REMOTE);
  await page.goto("/#/ledgers/tasks");

  // The embedded host still opens, and the console is usable. Refusing one row
  // must not cost the others — that is the property the whole multi-connection
  // slice exists for.
  await expect(page.getByRole("button", { name: "Add task" })).toHaveCount(1, {
    timeout: 30_000,
  });

  // The row that carried the refusal — and its "not encrypted" title, which is
  // what told the operator not to go looking at a working network — is hidden
  // with the roster (`src/product-scope.ts`). The refusal itself is unchanged
  // and is still asserted where it matters most: nothing was sent.
  await expect(page.getByTestId("host-switcher")).toHaveAttribute("data-worst-status", "down");

  // And nothing was sent there. The status is not a label applied after a round
  // trip; the round trip is what must not happen.
  const requested = await page.evaluate(() => window.__ocRequested ?? []);
  expect(requested).not.toContain(INSECURE_REMOTE);
});

test("an unencrypted host with no credential still connects", async ({ page, baseURL }) => {
  // The other half of the rule, and the reason it gates on the credential
  // rather than on the scheme: a home-lab or staging box without a certificate
  // stays usable. Nothing is exposed by reading it that a passer-by could not
  // have asked the host for themselves.
  //
  // Registered against the suite's own host so it genuinely answers; what is
  // under test is that an anonymous connection is not caught by #731's rule,
  // not that an arbitrary LAN address is reachable from CI.
  const host = new URL(baseURL ?? "http://127.0.0.1:8123").origin;
  await asDesktop(page, { embedded: null });
  await page.addInitScript((remote: string) => {
    window.localStorage.setItem(
      "oc.connections.v1",
      JSON.stringify([
        {
          id: "conn-anonymous-http",
          baseUrl: remote,
          label: "Anonymous over http",
          defaultCompany: null,
          credential: { kind: "cookie" },
        },
      ]),
    );
  }, host);
  await page.goto("/#/ledgers/tasks");

  // Read off the closed trigger: one host, so its state is the worst state.
  await expect(page.getByTestId("host-switcher")).toHaveAttribute("data-worst-status", "live", {
    timeout: 30_000,
  });
});

test("a desktop whose host did not start offers to start it, not a choice of where", async ({
  page,
}) => {
  // No embedded host, no remembered hosts: the state that used to render an
  // empty pane once the same-origin connection stopped filling it.
  await asDesktop(page, { embedded: null });
  await page.goto("/");

  await expect(page.getByTestId("no-connection")).toBeVisible({ timeout: 30_000 });

  // One machine runs one host, so the recovery is an action rather than a
  // four-way chooser between this computer, the cloud, a gateway and ssh.
  await expect(page.getByTestId("no-connection-run-here")).toBeVisible();
  await expect(page.getByTestId("no-connection-run-here")).toHaveText(
    "Start the host on this computer",
  );
  await expect(page.getByTestId("no-connection-add")).toHaveCount(0);

  // And the copy stops offering the alternative it no longer has.
  await expect(page.getByTestId("no-connection")).not.toContainText("somewhere else");
});
