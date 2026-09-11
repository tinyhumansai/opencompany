// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, expect, it } from "vitest";

import type { OpenCompanyClient } from "@/api/client";
import type { CompanyCredentialStatus } from "@/api/credential";
import type { InferenceStatus } from "@/api/inference";
import { InferenceSection } from "@/views/connections/InferenceSection";

/**
 * The console half of the managed round trip.
 *
 * `seedFromStatus` takes `status.provider` verbatim — deliberately, because it
 * is what stops the provider select and the header beside it from ever naming
 * two different providers. That makes the card completely dependent on the host
 * answering with the kind that was *chosen* rather than the kind it resolves to:
 * while `GET …/inference` folded `managed` onto its `openrouter` alias, saving
 * "Managed (TinyHumans)" snapped the select straight back to OpenRouter and took
 * the Connect-TinyHumans button — which only the managed route renders — with
 * it. The save had landed; nothing on screen could show it.
 *
 * `inference.spec.ts` proves the round trip against a real host end to end. This
 * pins the console's side of it, in a lane that runs on every push.
 */

let container: HTMLDivElement;
let root: Root;

function status(provider: string): InferenceStatus {
  return {
    provider,
    slug: "subscription",
    proxied: true,
    baseUrl: "https://api.tinyhumans.ai/openai/v1",
    models: {},
    defaultTierModels: {},
    source: "runtime",
    keyConfigured: false,
    cognition: "harness",
    usageMetering: "perTurn",
    restartRequired: false,
    harnessReachable: true,
    canRebuildInPlace: true,
  };
}

const CREDENTIAL: CompanyCredentialStatus = {
  configured: false,
  source: "attested",
  notice: "notice",
  // The Connect button needs a host with a hub wired; that is a separate
  // precondition from the one under test.
  hubLink: true,
};

/** A host that answers every read with `provider`, as a real one now does. */
function clientReporting(provider: string): OpenCompanyClient {
  return {
    scopeFor: () => "/api/v1/companies/acme",
    get: async (path: string) => {
      if (path.endsWith("/credential")) return CREDENTIAL;
      if (path.endsWith("/inference/models")) return { models: [], tierDefaults: {} };
      return status(provider);
    },
  } as unknown as OpenCompanyClient;
}

async function mount(client: OpenCompanyClient) {
  await act(async () => {
    root.render(createElement(InferenceSection, { client, company: "acme", canManage: true }));
  });
  await act(async () => {});
  await act(async () => {});
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

it("rests on the managed route, and offers its Connect button, when the host reports one", async () => {
  await mount(clientReporting("managed"));

  expect(container.querySelector('[data-testid="inference-current-provider"]')?.textContent).toBe(
    "Managed (TinyHumans)",
  );
  expect(container.querySelector("#inference-provider")?.textContent).toContain(
    "Managed (TinyHumans)",
  );
  expect(container.querySelector('[data-testid="connect-tinyhumans"]')).not.toBeNull();
});

it("does not offer the managed Connect button on a route that cannot be granted a key", async () => {
  // The other half of the pair: the button is the managed route's, not every
  // route's — TinyHumans can mint a key for its own account and not for an
  // OpenRouter one, so a card that showed it here would offer a step that
  // cannot apply. This is what makes the assertion above mean "managed" rather
  // than "the button renders regardless".
  await mount(clientReporting("openrouter"));

  expect(container.querySelector('[data-testid="inference-current-provider"]')?.textContent).toBe(
    "OpenRouter",
  );
  expect(container.querySelector('[data-testid="connect-tinyhumans"]')).toBeNull();
});
