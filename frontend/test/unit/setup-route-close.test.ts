// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { OpenCompanyClient } from "@/api/client";
import { ConnectionScopeProvider } from "@/connections/ConnectionContext";
import { SetupController } from "@/setup/SetupController";

/**
 * A `#/setup` address is a manual recovery path, and the blocking dialog it
 * opens must not survive the address.
 *
 * The shell clears `force` the moment `SetupController` consumes it, so `open`
 * would otherwise linger: a Back pressed from `#/setup` would leave the dialog
 * over Settings while the address bar says Settings (issue #1417 review). The
 * close is driven by `routeOpen` flipping false — and only by that edge, so a
 * dialog the first-run gate or the Team prompt opened (no `#/setup` involved)
 * is never yanked away by the route being merely absent.
 */

function clientWith(): OpenCompanyClient {
  return {
    listTeam: async () => [],
  } as unknown as OpenCompanyClient;
}

let container: HTMLDivElement;
let root: Root;
// Stable across re-renders: the roster gate effect depends on `client` and the
// scope, and a fresh identity on either would re-run it — which resets `open`
// — and look like a route-driven close.
const client = clientWith();
const scope = { connection: "conn-test", company: "acme" as string | null };

async function show(props: {
  force?: boolean;
  routeOpen?: boolean;
  onRouteDismiss?: () => void;
} = {}) {
  await act(async () => {
    root.render(
      createElement(
        ConnectionScopeProvider,
        { scope, children: createElement(SetupController, {
          client,
          company: scope.company,
          // The automatic first-run gate is a different concern from the route;
          // deep-linking suppresses it so `force` is the only thing that opens.
          deepLinked: true,
          onOpenChange: () => {},
          ...props,
        }) },
      ),
    );
  });
}

beforeEach(() => {
  (globalThis as unknown as { IS_REACT_ACT_ENVIRONMENT: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
});

// The dialog renders through a portal, so it lives under `document.body`, not
// the mount container.
const dialog = () => document.body.querySelector('[data-testid="setup-dialog"]');

describe("leaving the #/setup route", () => {
  it("closes the dialog the route opened", async () => {
    await show({ force: true, routeOpen: true });
    expect(dialog()).toBeTruthy();

    // Back from `#/setup`: the shell has cleared force (onForceHandled) and the
    // address now names Settings.
    await show({ force: false, routeOpen: false });
    expect(dialog()).toBeNull();
  });

  it("navigates away when the route is dismissed", async () => {
    const onRouteDismiss = vi.fn();
    await show({ force: true, routeOpen: true, onRouteDismiss });
    expect(dialog()).toBeTruthy();

    const skip = document.body.querySelector('[data-testid="setup-skip"]');
    expect(skip).toBeTruthy();
    await act(async () => {
      (skip as HTMLButtonElement).click();
    });

    expect(onRouteDismiss).toHaveBeenCalledOnce();
  });

  it("does not close a Team-prompt dialog just because the route is absent", async () => {
    // The Team page's prompt opens setup with no `#/setup` involved; `force`
    // is the only opener, exactly as in the first-run-gate-adjacent flow.
    await show({ force: true, routeOpen: false });
    expect(dialog()).toBeTruthy();

    await show({ force: false, routeOpen: false });
    expect(dialog()).toBeTruthy();
  });
});
