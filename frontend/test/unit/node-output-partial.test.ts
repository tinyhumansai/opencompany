// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it } from "vitest";

import { OutputSection } from "@/views/workflows/NodeDetailPanel";
import type { NodeOutputView } from "@/views/workflows/run-output";

/**
 * The node inspector's Output section (issue #1008).
 *
 * A failed or blocked run now persists the per-node output it reached, flagged
 * `partial`. The inspector must badge that capture as partial rather than
 * silently rendering it as a clean result — and the old "this run predates
 * output capture" empty state must fire ONLY for a genuinely missing snapshot
 * (`unavailable`), never for a node that has present output.
 */

let container: HTMLDivElement;
let root: Root;

function render(output: NodeOutputView) {
  act(() => {
    root.render(createElement(OutputSection, { output }));
  });
}

beforeEach(() => {
  (globalThis as unknown as { IS_REACT_ACT_ENVIRONMENT: boolean }).IS_REACT_ACT_ENVIRONMENT =
    true;
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
});

const presentValue = { items: [{ json: { text: "the draft" } }] };

describe("OutputSection partial badge (issue #1008)", () => {
  it("renders the partial-capture badge when the snapshot is partial", () => {
    render({ state: "present", value: presentValue, truncated: false, partial: true });
    expect(container.querySelector('[data-testid="node-output-partial"]')).not.toBeNull();
    // A present node NEVER shows the "predates output capture" empty state.
    expect(container.querySelector('[data-testid="node-output-empty"]')).toBeNull();
  });

  it("omits the partial badge for a clean settled capture", () => {
    render({ state: "present", value: presentValue, truncated: false, partial: false });
    expect(container.querySelector('[data-testid="node-output-partial"]')).toBeNull();
    expect(container.querySelector('[data-testid="node-output"]')).not.toBeNull();
  });

  it("omits the partial badge when the field is absent (a live run's result)", () => {
    render({ state: "present", value: presentValue, truncated: false });
    expect(container.querySelector('[data-testid="node-output-partial"]')).toBeNull();
  });

  it("shows the predates-capture empty state only for a genuinely missing snapshot", () => {
    render({ state: "unavailable" });
    const empty = container.querySelector('[data-testid="node-output-empty"]');
    expect(empty).not.toBeNull();
    expect(empty?.textContent).toContain("predates output capture");
    expect(container.querySelector('[data-testid="node-output-partial"]')).toBeNull();
  });

  it("surfaces a partial node's run artifacts even when it returned no text", () => {
    render({
      state: "present",
      value: {
        items: [],
        artifacts: [
          {
            source: "reports/partial.md",
            title: "partial.md",
            kind: "markdown",
            workspaceNodeId: "node-42",
          },
        ],
      },
      truncated: false,
      partial: true,
    });

    const artifact = container.querySelector<HTMLAnchorElement>(
      '[data-testid="node-output-artifact"]',
    );
    expect(artifact?.textContent).toContain("partial.md");
    expect(artifact?.getAttribute("href")).toBe("#/workspace/node-42");
    expect(container.querySelector('[data-testid="node-output-none"]')).toBeNull();
  });
});
