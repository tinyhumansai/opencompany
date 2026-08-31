import { readFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import { describe, expect, it } from "vitest";

import { VIEWS } from "@/lib/console-routes";

const here = dirname(fileURLToPath(import.meta.url));
const read = (rel: string) => readFileSync(resolve(here, "../../src", rel), "utf8");

describe("first-run setup recovery (issue #1417)", () => {
  it("keeps the manual setup address routable", () => {
    expect(VIEWS).toContain("setup");
  });

  it("offers the same route beside the product-tour replay control", () => {
    const settings = read("views/SettingsView.tsx");

    // The anchor must carry the host scope (`withHostParam`) so a Ctrl/Cmd-click
    // into a fresh tab opens setup on the company the operator was looking at,
    // not on the bootstrap/default host (issue #1417 review).
    expect(settings).toContain('withHostParam("setup")');
    expect(settings.indexOf('withHostParam("setup")')).toBeGreaterThan(
      settings.indexOf("Replay tour"),
    );
  });

  it("keeps the not-found page's Overview anchor on the active host", () => {
    const unknown = read("views/UnknownRouteView.tsx");

    expect(unknown).toContain('withHostParam("overview")');
  });
});
