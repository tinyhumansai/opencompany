// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it } from "vitest";

import type { OpenCompanyClient } from "@/api/client";
import type { WorkflowGraph } from "@/api/workflows";
import { CopilotPanel } from "@/views/workflows/CopilotPanel";

/**
 * Issue #874. The copilot's grounding must not carry one company's tool wiring
 * into another company's prompt.
 *
 * This suite is normally for pure functions; the exception is earned the same
 * way `billing-company-switch` earns it. What is under test is not a function
 * anybody can call — it is the *window* between a company switch and the new
 * company's tool-slug read landing. The panel holds the effective slugs and the
 * granted-but-unwired advisory in two pieces of state, and only a mounted,
 * rendering component has that pair in the half-updated condition the bug lives
 * in.
 *
 * The failure it pins: the reset that runs on a company change cleared the
 * slugs but not the unwired list, so for the whole in-flight window the
 * composed message told the model "the granted tools could not be listed here"
 * while, immediately below, naming the PREVIOUS company's tools as ones it must
 * not author. Self-contradictory as a prompt, and a leak of one company's
 * deployment wiring into another's turn.
 */

const graph: WorkflowGraph = {
  id: "weekly_report",
  name: "Weekly report",
  version: null,
  description: "Assemble and send the Monday summary.",
  nodes: [
    {
      id: "collect",
      kind: "agent",
      name: "Collect",
      agent: "analyst",
      summary: "Pull last week's numbers",
    },
  ],
  edges: [],
};

/** Every message the panel actually sent, in order. */
let sent: string[];

/**
 * One client for the whole session, as the app has: the effect that re-reads
 * the grounding then re-runs on the COMPANY changing, which is the transition
 * under test — a fresh client per company would re-run it for the wrong reason.
 *
 * `acme` is fully answered. `globex` answers the inference read (so the
 * composer opens) but leaves the tool-slug read hanging — that is the in-flight
 * window, held open for the length of the test.
 */
function client(): OpenCompanyClient {
  return {
    scopeFor: (company: string | null) => `/api/v1/companies/${company}`,
    get: async (path: string) => {
      if (path.endsWith("/inference")) return { cognition: "hosted" };
      if (path.endsWith("/workflows/tool-slugs")) {
        if (path.includes("/companies/acme/")) {
          return {
            slugs: ["send_email"],
            // A slug the composed message does not already contain: the schema
            // example uses `web_search`, so it would be present either way and
            // the absence assertion below could not tell the two apart.
            unwired: [
              {
                slug: "deep_research",
                reason: "searchBackendNotConfigured",
                detail: "granted, but no managed search backend is configured",
              },
            ],
          };
        }
        // Still in flight, for the rest of the test.
        return new Promise(() => {});
      }
      throw new Error(`unexpected read: ${path}`);
    },
    listTeam: async () => [],
    getChatHistory: async () => [],
    chat: async (message: string) => {
      sent.push(message);
      return { responses: [{ text: "ok" }] };
    },
  } as unknown as OpenCompanyClient;
}

let container: HTMLDivElement;
let root: Root;
let panelClient: OpenCompanyClient;

/** Renders the panel for a company, WITHOUT remounting it — the real switch. */
async function showCopilot(company: string) {
  await act(async () => {
    root.render(
      createElement(CopilotPanel, {
        client: panelClient,
        company,
        graph,
        runs: [],
        runsKnown: true,
        runsReady: true,
        onApplied: () => {},
        onClose: () => {},
      }),
    );
  });
}

/** Asks a question the way an operator does, and returns what was sent. */
async function ask(question: string): Promise<string> {
  const box = container.querySelector<HTMLTextAreaElement>(
    '[data-testid="workflow-copilot-input"]',
  );
  if (!box) throw new Error("the copilot composer is not on the page");
  await act(async () => {
    const setter = Object.getOwnPropertyDescriptor(
      window.HTMLTextAreaElement.prototype,
      "value",
    )?.set;
    setter?.call(box, question);
    box.dispatchEvent(new Event("input", { bubbles: true }));
  });
  const send = container.querySelector<HTMLButtonElement>(
    '[data-testid="workflow-copilot-send"]',
  );
  if (!send) throw new Error("the copilot send button is not on the page");
  if (send.disabled) throw new Error("the copilot refused to send");
  await act(async () => {
    send.click();
  });
  const message = sent.at(-1);
  if (message === undefined) throw new Error("nothing was sent");
  return message;
}

beforeEach(() => {
  (globalThis as unknown as { IS_REACT_ACT_ENVIRONMENT: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
  sent = [];
  panelClient = client();
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
});

describe("the workflow copilot's grounding across a company switch", () => {
  it("does not carry the previous company's unwired tools into the next one", async () => {
    await showCopilot("acme");
    // Acme's own turn names them, which is the #874 behaviour being protected —
    // if this stops holding, the test below would pass for the wrong reason.
    const forAcme = await ask("add a research step");
    expect(forAcme).toMatch(/granted but NOT wired/i);
    expect(forAcme).toContain("deep_research");

    // The operator switches company. Globex's tool-slug read is still in
    // flight, so this turn is composed in the half-updated window.
    await showCopilot("globex");
    const forGlobex = await ask("add a research step");

    expect(forGlobex).toMatch(/could not be listed/i);
    expect(forGlobex).not.toContain("deep_research");
    expect(forGlobex).not.toMatch(/granted but NOT wired/i);
  });
});
