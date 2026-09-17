import { readFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
// @vitest-environment jsdom

import { beforeEach, describe, expect, it } from "vitest";

const frontendRoot = resolve(dirname(fileURLToPath(import.meta.url)), "../..");
const indexHtml = readFileSync(resolve(frontendRoot, "index.html"), "utf8");
const loader = readFileSync(resolve(frontendRoot, "public/openpanel-init.js"), "utf8");
const tauriConfig = readFileSync(
  resolve(frontendRoot, "../crates/opencompany-app/tauri.conf.json"),
  "utf8",
);
const tauriManifest = JSON.parse(tauriConfig) as {
  app: { security: { csp: string } };
};

function cspSources(csp: string, directiveName: string): string[] {
  const directive = csp
    .split(";")
    .map((part) => part.trim())
    .find((part) => part.startsWith(`${directiveName} `));
  return directive?.split(/\s+/).slice(1) ?? [];
}

function sourceAllowsOrigin(source: string, origin: URL): boolean {
  if (source === "*" || source === origin.protocol) return true;
  if (!source.startsWith(`${origin.protocol}//`)) return false;
  const hostname = source.slice(`${origin.protocol}//`.length).split(/[/:]/, 1)[0];
  return hostname === origin.hostname || (hostname.startsWith("*.") && origin.hostname.endsWith(hostname.slice(1)));
}

describe("OpenPanel console analytics", () => {
  beforeEach(() => {
    delete window.op;
    delete (window as Window & { __TAURI_INTERNALS__?: unknown }).__TAURI_INTERNALS__;
    delete window.OPENCOMPANY_CONFIG;
    document.head.querySelectorAll('script[src="https://openpanel.dev/op1.js"]').forEach((script) => {
      script.remove();
    });
  });

  function runLoader(analytics?: boolean, analyticsEndpoint?: string): void {
    if (analytics === undefined) {
      new Function(loader)();
      return;
    }
    Object.defineProperty(window, "OPENCOMPANY_CONFIG", {
      configurable: true,
      value: { analytics, analyticsEndpoint },
    });
    new Function(loader)();
  }

  it("does not install a client or script without explicit opt-in", () => {
    runLoader();

    expect(window.op).toBeUndefined();
    expect(document.head.querySelector('script[src="https://openpanel.dev/op1.js"]')).toBeNull();
  });

  it("stays silent when analytics is explicitly disabled", () => {
    runLoader(false);

    expect(window.op).toBeUndefined();
    expect(document.head.querySelector('script[src="https://openpanel.dev/op1.js"]')).toBeNull();
  });

  it("stays silent in the Tauri desktop webview", () => {
    Object.defineProperty(window, "__TAURI_INTERNALS__", {
      configurable: true,
      value: {},
    });

    runLoader(true, "https://collector.example/track");

    expect(window.op).toBeUndefined();
    expect(document.head.querySelector('script[src="https://openpanel.dev/op1.js"]')).toBeNull();
  });

  it("installs the configured client and script after explicit opt-in", () => {
    runLoader(true, "https://collector.example/track");

    expect(window.op).toBeDefined();
    expect(window.op?.q).toContainEqual([
      "init",
      {
        apiUrl: "https://collector.example/track",
        clientId: "afe8ec4e-0a6a-427a-aa22-49cbbf137d0a",
        trackScreenViews: false,
        trackOutgoingLinks: false,
        trackAttributes: false,
      },
    ]);
    expect(document.head.querySelector('script[src="https://openpanel.dev/op1.js"]')).not.toBeNull();
  });

  it("stays silent when opt-in has no configured collector endpoint", () => {
    runLoader(true);

    expect(window.op).toBeUndefined();
    expect(document.head.querySelector('script[src="https://openpanel.dev/op1.js"]')).toBeNull();
  });

  it("loads the configured browser client only after explicit opt-in", () => {
    expect(indexHtml).toContain('src="/openpanel-init.js"');
    expect(loader).toContain('src = "https://openpanel.dev/op1.js"');
    expect(loader).toContain("apiUrl: window.OPENCOMPANY_CONFIG.analyticsEndpoint");
    expect(loader).toContain('clientId: "afe8ec4e-0a6a-427a-aa22-49cbbf137d0a"');
    expect(loader).toContain("window.OPENCOMPANY_CONFIG?.analytics !== true");
    expect(loader).toContain("trackScreenViews: false");
    expect(loader).toContain("trackOutgoingLinks: false");
    expect(loader).toContain("trackAttributes: false");
    expect(loader).toContain('window.op("init"');
  });

  it("permits exactly the required OpenPanel origins in the desktop webview", () => {
    const csp = tauriManifest.app.security.csp;
    const scriptSources = cspSources(csp, "script-src");
    const connectSources = cspSources(csp, "connect-src");
    const openPanelOrigin = new URL("https://openpanel.dev");

    expect(scriptSources).toContain("'self'");
    // Sentry's ingest origin rides here too (#2380). It is listed rather than
    // matched loosely so that widening the webview's reach stays a deliberate
    // edit to this line — which is the whole point of asserting the set
    // exactly. The two PRs that landed these facts could not see each other:
    // #2377 wrote this expectation, #2380 added the origin, and the Console
    // lane is path-filtered, so `main` never ran the two together.
    expect(connectSources).toEqual([
      "'self'",
      "ipc:",
      "http://ipc.localhost",
      "https://sentry.tinyhumans.ai",
    ]);
    expect(scriptSources.some((source) => sourceAllowsOrigin(source, openPanelOrigin))).toBe(false);
    expect(connectSources.some((source) => sourceAllowsOrigin(source, openPanelOrigin))).toBe(false);
  });
});
