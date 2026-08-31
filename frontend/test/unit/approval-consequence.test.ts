// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it } from "vitest";

import type { ApprovalSummary } from "@/api/types";
import { ApprovalHeadline } from "@/components/approval-card";

/**
 * Issue #1426: outward and irreversible requests must not look like routine
 * internal work. The host classifies the consequence, so this checks the
 * rendered shared headline rather than inferring it from the effect kind.
 */
function approval(overrides: Partial<ApprovalSummary> = {}): ApprovalSummary {
  return {
    id: "a1",
    kind: "composio_execute",
    amount_usd: null,
    at_millis: 0,
    ...overrides,
  } as ApprovalSummary;
}

let container: HTMLDivElement;
let root: Root;

async function render(a: ApprovalSummary) {
  await act(async () => {
    root.render(createElement(ApprovalHeadline, { approval: a }));
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

describe("the consequence on an approval card", () => {
  it.each([
    ["spend", "Spends money", "bg-tone-4\\/15"],
    ["send", "Leaves the company", "bg-tone-2\\/15"],
    ["sign", "Makes a commitment", "bg-tone-1\\/15"],
    ["publish", "Publishes work", "bg-tone-3\\/15"],
    ["hire", "Engages or drops a counterparty", "bg-tone-4\\/15"],
    ["identity", "Changes its identity or keys", "bg-tone-5\\/15"],
  ] as const)("marks %s approvals as %s", async (group, label, iconClass) => {
    await render(approval({ group }));

    expect(container.textContent).toContain(label);
    expect(container.querySelector(`.${iconClass}`)).not.toBeNull();
  });

  it("leaves internal and old-host approvals unmarked", async () => {
    await render(approval({ group: "other" }));
    expect(container.textContent).not.toContain("Spends money");
    expect(container.textContent).not.toContain("Leaves the company");
    expect(container.querySelector(".bg-muted.text-foreground")).not.toBeNull();

    await render(approval());
    expect(container.textContent).not.toContain("Spends money");
    expect(container.textContent).not.toContain("Leaves the company");
    expect(container.querySelector(".bg-muted.text-foreground")).not.toBeNull();
  });

  // The `publish` group covers `publish_artifact`, which writes only into the
  // company's own workspace and artifact chain and sends nothing anywhere, as
  // well as the genuinely external `repo_publish` and `hosting_*` effects. A
  // label claiming the first of those goes public is the misleading-label
  // failure `language.ts` refuses for the same tool, so it stays refused here.
  it("does not claim a publish approval goes public", async () => {
    await render(approval({ group: "publish" }));
    expect(container.textContent).not.toMatch(/public/i);
  });

  // `docs/brand/README.md` ("Status is a closed vocabulary") reserves the five
  // status hues for run state. A consequence is a category, not a run outcome,
  // so a pending approval must never be painted the green that means "finished
  // cleanly" or the red that means "failed".
  it.each(["spend", "send", "sign", "publish", "hire", "identity"] as const)(
    "tints a %s consequence from the identity palette, not the status one",
    async (group) => {
      await render(approval({ group }));
      expect(container.innerHTML).not.toMatch(/status-(done|failed|running|blocked|idle)-soft/);
    },
  );

  // `hire` and `identity` are separate rows in the taxonomy and shared one
  // label until #1426, which hid the distinction the badge exists to draw.
  it("labels hire and identity approvals differently", async () => {
    await render(approval({ group: "hire" }));
    const hire = container.textContent ?? "";

    await render(approval({ group: "identity" }));
    const identity = container.textContent ?? "";

    expect(hire).not.toEqual(identity);
  });
});
