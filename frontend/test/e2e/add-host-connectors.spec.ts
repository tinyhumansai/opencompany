import { expect, test, type Page } from "@playwright/test";

import { openHostMenu } from "./host-switcher";

/**
 * Choosing where a runtime runs, driven through the whole app.
 *
 * The unit tests drive the connector model directly — what a profile means,
 * what the store refuses, when a failed probe is worth another attempt. This
 * drives the console the way a person does, because everything under test here
 * lives in the wiring rather than in an export: which tabs a runtime offers,
 * what the shell is asked for when one is chosen, and what the roster says
 * afterwards.
 *
 * See `docs/spec/runtime/connectors.md`.
 *
 * ## What is shimmed, and what is not
 *
 * `window.__TAURI__` exists only inside the packaged shell, so the bridge is
 * shimmed and nothing else — the same shape `desktop-local-instances.spec.ts`
 * uses, plus the three tunnel commands. It answers `oc_open_ssh_tunnel` from a
 * script the spec holds, which is what `SshTunnels` does in Rust minus the
 * child process. **No `ssh` runs here**: a spec that needs a reachable machine
 * with a key on it is a spec CI skips, and a skipped spec proves nothing.
 * `ssh.rs`'s own tests cover the argv and the roster; every decision asserted
 * on here belongs to the console.
 *
 * The browser cases shim nothing at all.
 */

/** What the shimmed core does when the console asks for a tunnel. */
interface TunnelScript {
  /** The address the tunnel forwards to, as `oc_open_ssh_tunnel` reports it. */
  baseUrl?: string;
  /** What `ssh` said, when it said no. */
  refuse?: string;
}

interface Instance {
  id: string;
  label: string;
  dataDir: string;
  running: boolean;
  baseUrl?: string;
  instanceId?: string;
  companies?: string[];
}

/** The single live local host every desktop case here starts from. */
function seed(liveBaseUrl: string): Instance[] {
  return [
    {
      id: "default",
      label: "This computer",
      dataDir: "/tmp/e2e-connectors",
      running: true,
      baseUrl: liveBaseUrl,
      instanceId: "instance-default",
      companies: [],
    },
  ];
}

/**
 * Installs a bridge over a roster and a tunnel script the page can read.
 *
 * `opened` records every target the console asked for, because *what the shell
 * is asked* is half of what these specs are about: the console must send the
 * destination someone typed, not a url it made up.
 */
async function installDesktopShell(
  page: Page,
  roster: Instance[],
  tunnel: TunnelScript,
): Promise<void> {
  await page.addInitScript(
    ([seedRoster, script]: [Instance[], TunnelScript]) => {
      // The tour modal covers the console and swallows clicks.
      for (const key of ["oc-tour:single", "oc-tour:e2e-harness-co", "oc-tour:null"]) {
        window.localStorage.setItem(key, JSON.stringify({ skipped: true, seenAt: Date.now() }));
      }

      const instances: Instance[] = JSON.parse(JSON.stringify(seedRoster)) as Instance[];
      const hosts = new Map<string, string>();
      const opened: unknown[] = [];
      (window as unknown as { __OPENED_TUNNELS__: unknown[] }).__OPENED_TUNNELS__ = opened;

      async function proxy(
        connectionId: string,
        request: {
          method: string;
          path: string;
          headers?: Record<string, string>;
          body?: string | null;
        },
      ) {
        const base = hosts.get(connectionId) ?? "";
        const response = await fetch(`${base}${request.path}`, {
          method: request.method,
          headers: request.headers,
          body: request.body ?? undefined,
          credentials: "include",
        });
        const headers: Record<string, string> = {};
        response.headers.forEach((value, key) => {
          headers[key.toLowerCase()] = value;
        });
        return {
          status: response.status,
          statusText: response.statusText,
          url: response.url,
          text: await response.text(),
          headers,
        };
      }

      (window as unknown as { __TAURI__: unknown }).__TAURI__ = {
        core: {
          invoke(command: string, args: Record<string, unknown> = {}) {
            switch (command) {
              case "oc_local_instances":
                return Promise.resolve(JSON.parse(JSON.stringify(instances)) as Instance[]);
              case "oc_open_ssh_tunnel": {
                opened.push(args.target);
                if (script.refuse) return Promise.reject(new Error(script.refuse));
                const target = args.target as { destination: string; remotePort: number };
                return Promise.resolve({
                  id: `${target.destination}:22:${target.remotePort}`,
                  destination: target.destination,
                  remotePort: target.remotePort,
                  baseUrl: script.baseUrl,
                });
              }
              case "oc_close_ssh_tunnel":
              case "oc_ssh_tunnels":
                return Promise.resolve([]);
              case "oc_connect":
                hosts.set(args.connectionId as string, args.baseUrl as string);
                return Promise.resolve();
              case "oc_disconnect":
                hosts.delete(args.connectionId as string);
                return Promise.resolve();
              case "oc_connections":
                return Promise.resolve([...hosts.keys()]);
              case "oc_request":
                return proxy(
                  args.connectionId as string,
                  args.request as unknown as Parameters<typeof proxy>[1],
                );
              case "oc_subscribe":
                return Promise.resolve();
              default:
                return Promise.resolve(undefined);
            }
          },
          Channel: class {
            onmessage: unknown = null;
          },
        },
      };
    },
    [roster, tunnel] as [Instance[], TunnelScript],
  );
}

/** Opens "Add a host" and waits for the chooser. */
async function openTheChooser(page: Page): Promise<void> {
  const chooser = page.getByTestId("add-host-remote");
  // Choosing "Add a host" closes the menu the item lives in, so the click can
  // observe its own target detaching and then retry against a menu that is
  // already gone. The chooser's arrival is the assertion — drive the
  // open-and-choose pair until it lands, same shape as `clickClearOfToasts`.
  await expect(async () => {
    await openHostMenu(page);
    await page.getByTestId("host-switcher-add").click({ timeout: 2_000 }).catch(() => {
      // The menu closed under the click; the chooser check below is the verdict.
    });
    await expect(chooser).toBeVisible({ timeout: 2_000 });
  }).toPass({ timeout: 15_000, intervals: [250] });
}

/** Every connector this console is offering right now. */
async function offeredConnectors(page: Page): Promise<string[]> {
  const kinds = ["local", "cloud", "remote", "ssh"];
  const present = await Promise.all(
    kinds.map(async (kind) => ((await page.getByTestId(`add-host-${kind}`).count()) > 0 ? kind : null)),
  );
  return present.filter((kind): kind is string => kind !== null);
}

/** What the console wrote down about the hosts it holds. */
async function storedConnectors(page: Page): Promise<unknown[]> {
  return page.evaluate(() => {
    const raw = window.localStorage.getItem("oc.connections.v1");
    return raw
      ? (JSON.parse(raw) as { connector?: unknown }[]).map((profile) => profile.connector)
      : [];
  });
}

const switcher = (page: Page) => page.getByTestId("host-switcher");

test("a desktop offers all four places a runtime can run", async ({ page, baseURL }) => {
  await installDesktopShell(page, seed(baseURL ?? ""), {});
  await page.goto("/");
  await openTheChooser(page);

  expect(await offeredConnectors(page)).toEqual(["local", "cloud", "remote", "ssh"]);
});

/**
 * The chooser is a screen of the onboarding flow, not a popup over the console
 * ([#1531](https://github.com/tinyhumansai/opencompany/issues/1531)).
 *
 * It was a `Dialog`, and the dialog is 24rem wide: four connector tabs clipped
 * their own labels inside it. Asserting the *symptom* — every tab strip fitting
 * the box it is drawn in — rather than the absence of a `role=dialog`, because
 * the width is what a person reports and a future card that is too narrow again
 * should fail here.
 */
test("the chooser is a screen, and its tabs fit the card they are drawn in", async ({
  page,
  baseURL,
}) => {
  await installDesktopShell(page, seed(baseURL ?? ""), {});
  await page.goto("/");
  await openTheChooser(page);

  const card = page.getByTestId("add-host");
  await expect(card).toBeVisible();
  // Its title stays put rather than scrolling away to make room for the form,
  // which is the other half of what the dialog could not do.
  await expect(card.getByRole("heading", { name: "Add a host" })).toBeVisible();

  const strip = card.locator("[data-slot=tabs-list]").first();
  const stripBox = await strip.boundingBox();
  expect(stripBox, "the tab strip should have a box").not.toBeNull();
  for (const kind of ["local", "cloud", "remote", "ssh"]) {
    const tab = page.getByTestId(`add-host-${kind}`);
    const box = await tab.boundingBox();
    expect(box, `${kind} should have a box`).not.toBeNull();
    expect(box!.x, `${kind} starts inside the strip`).toBeGreaterThanOrEqual(stripBox!.x - 1);
    expect(
      box!.x + box!.width,
      `${kind} ends inside the strip`,
    ).toBeLessThanOrEqual(stripBox!.x + stripBox!.width + 1);
  }

  // And there is a way back, to the console that was there before — which is
  // still the console it was, rather than a re-booted one: a screen that
  // unmounted it would replay "Connecting…" here.
  await page.getByTestId("add-host-back").click();
  await expect(card).toHaveCount(0);
  await expect(switcher(page)).toHaveAttribute("data-host-count", "1");
});

test("a browser is offered only the two connectors it can honour", async ({ page }) => {
  // A hub, because it is the browser shape that genuinely holds N hosts, and
  // the connector list is about adding the *next* one. A single-host console
  // opens the same menu (`hostSwitcherMenu`), but only because it has a host
  // to manage — this spec is about the choice, so it drives the shape the
  // choice belongs to.
  await page.goto("/?hub");
  await openTheChooser(page);

  // `local` and `ssh` both need a process started on this machine, and a
  // browser has no core to start one in. A tab whose button cannot be honoured
  // is worse than a tab that is not there.
  expect(await offeredConnectors(page)).toEqual(["cloud", "remote"]);
});

test("a browser is told a gateway has to allow this console's origin", async ({ page }) => {
  // The most likely support question this connector generates, answered where
  // it is cheapest to answer. There is no wildcard for it either — the session
  // is a credential — so the operator has to go and set this.
  await page.goto("/?hub");
  await openTheChooser(page);
  await page.getByTestId("add-host-remote").click();

  await expect(page.getByText("OPENCOMPANY_CORS_ORIGINS")).toBeVisible();
});

test("a host reached over ssh is added at the address the shell opened", async ({
  page,
  baseURL,
}) => {
  // The tunnel forwards to the live harness host, which is what a real one
  // would do: the console addresses loopback and something answers there.
  await installDesktopShell(page, seed(baseURL ?? ""), { baseUrl: baseURL ?? "" });
  await page.goto("/");
  await expect(switcher(page)).toHaveAttribute("data-host-count", "1");

  await openTheChooser(page);
  await page.getByTestId("add-host-ssh").click();
  await page.getByLabel("Machine").fill("acme-vps");
  await page.getByRole("button", { name: "Connect" }).click();

  await expect(switcher(page)).toHaveAttribute("data-host-count", "2", { timeout: 30_000 });

  // The shell was asked for the machine someone typed, with the far side's
  // port defaulted rather than left undefined.
  //
  // Asked more than once, and that is the design rather than a slip: the
  // dialog opens the tunnel so a refusal lands in the form, and the probe that
  // follows asks again because *every* launch has to. Opening is idempotent
  // per target on the core's side, which is what makes asking twice free — so
  // what this asserts is that every ask names the same target.
  const asked = await page.evaluate(
    () => (window as unknown as { __OPENED_TUNNELS__: unknown[] }).__OPENED_TUNNELS__,
  );
  expect(asked.length).toBeGreaterThan(0);
  for (const target of asked) {
    expect(target).toEqual({ destination: "acme-vps", remotePort: 8080 });
  }

  // And the connector is written down. Nothing about `http://127.0.0.1:<port>`
  // says it is a tunnel, and the port is this launch's — so a console that did
  // not record the target could not reopen it, and would mint a fresh id and
  // orphan every scoped key under it next launch (issue #615).
  expect(await storedConnectors(page)).toContainEqual({
    kind: "ssh",
    target: { destination: "acme-vps", remotePort: 8080 },
  });
});

test("ssh's refusal stays on screen instead of becoming a host", async ({ page, baseURL }) => {
  await installDesktopShell(page, seed(baseURL ?? ""), {
    refuse: "Permission denied (publickey).",
  });
  await page.goto("/");
  await openTheChooser(page);
  await page.getByTestId("add-host-ssh").click();
  await page.getByLabel("Machine").fill("acme-vps");
  await page.getByRole("button", { name: "Connect" }).click();

  // `ssh`'s own words, in the dialog the operator is standing in front of.
  // Opening the tunnel here rather than leaving it to the first probe is what
  // buys that: the alternative is a red row they have to go and read, saying
  // "could not be reached" about a machine that answered and refused them.
  await expect(page.getByText("Permission denied (publickey).")).toBeVisible();

  // No host was added, and the dialog is still open to be corrected.
  await expect(switcher(page)).toHaveAttribute("data-host-count", "1");
  await expect(page.getByLabel("Machine")).toBeVisible();
});

/**
 * A dead address, for the two rows below. Port 9 is `discard`, which nothing
 * on a test runner is listening on.
 */
const ASLEEP = "http://127.0.0.1:9";

/** Seeds a second host at a dead address, as the given connector. */
async function seedSecondHost(page: Page, connector: unknown): Promise<void> {
  await page.addInitScript(
    ([dead, kind]: [string, unknown]) => {
      // The tour modal covers the console and swallows every click, including
      // the one that opens the switcher.
      for (const key of ["oc-tour:single", "oc-tour:e2e-harness-co", "oc-tour:null"]) {
        window.localStorage.setItem(key, JSON.stringify({ skipped: true, seenAt: Date.now() }));
      }
      window.localStorage.setItem(
        "oc.connections.v1",
        JSON.stringify([
          {
            id: "conn-primary",
            baseUrl: "",
            label: "Primary",
            defaultCompany: null,
            credential: { kind: "cookie" },
          },
          {
            id: "conn-second",
            baseUrl: dead,
            label: "Second",
            defaultCompany: null,
            credential: { kind: "cookie" },
            connector: kind,
          },
        ]),
      );
    },
    [ASLEEP, connector] as [string, unknown],
  );
}

test("a cloud tenant that is asleep is being woken, not unreachable", async ({ page }) => {
  // The platform starts an idle tenant when a request arrives, so its first
  // request takes seconds. Reporting that as a host that is gone tells an
  // operator their company is broken when it is merely asleep — and nothing
  // re-probes, so the row would stay wrong.
  await seedSecondHost(page, { kind: "cloud", tenant: "acme" });
  await page.goto("/");

  await expect(switcher(page)).toHaveAttribute("data-host-count", "2", { timeout: 30_000 });
  await openHostMenu(page);
  await expect(page.getByTestId("host-row-state-conn-second")).toHaveText("Waking…", {
    timeout: 30_000,
  });
  // In the same rank as any other connecting host: waking is not a fifth
  // status, it is a connecting one that says what it is doing.
  await expect(page.getByTestId("host-row-conn-second")).toHaveAttribute(
    "data-status",
    "connecting",
  );
});

test("the same address as a gateway you run is simply unreachable", async ({ page }) => {
  // The other half, and the reason the connector has to be written down: these
  // two rows are the same url and the same failure, and they mean different
  // things. Nothing is going to wake a gateway somebody runs themselves, so
  // waiting on it would be a spinner that never resolves.
  await seedSecondHost(page, { kind: "remote" });
  await page.goto("/");

  await expect(switcher(page)).toHaveAttribute("data-host-count", "2", { timeout: 30_000 });
  await openHostMenu(page);
  await expect(page.getByTestId("host-row-state-conn-second")).toHaveText("Unreachable", {
    timeout: 30_000,
  });
});

test("a hub with no hosts offers the choice rather than describing it", async ({ page }) => {
  // The onboarding dead end. A hub's own origin serves assets and nothing
  // else, so a new one holds zero connections — nothing went wrong, and the
  // desktop's "the host on this computer didn't start" describes a computer
  // that was never going to run one.
  await page.goto("/?hub");

  const empty = page.getByTestId("no-connection");
  await expect(empty).toBeVisible({ timeout: 30_000 });
  await expect(empty).toContainText("No company connected yet");
  await expect(empty).not.toContainText("didn't start");

  // And it is a control, not a sentence naming one somewhere else.
  await page.getByTestId("no-connection-add").click();
  expect(await offeredConnectors(page)).toEqual(["cloud", "remote"]);
});
