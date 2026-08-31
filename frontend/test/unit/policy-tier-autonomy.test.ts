// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { OpenCompanyClient } from "@/api/client";
import type { PolicyStatus } from "@/api/policy";
import { widensAutonomy, widensSpendCap, gatedBy } from "@/components/policy-settings";

const toasts = vi.hoisted(() => ({
  base: vi.fn(),
  success: vi.fn(),
  error: vi.fn(),
  warning: vi.fn(),
  info: vi.fn(),
}));

vi.mock("sonner", () => {
  const toast = Object.assign(toasts.base, toasts);
  return { toast };
});

const { PolicySettings } = await import("@/components/policy-settings");

const TIERS = [
  {
    value: "readonly",
    label: "Read-only",
    description: "The agents can look at things but change nothing and spend nothing.",
  },
  {
    value: "supervised",
    label: "Supervised",
    description: "Conservative execution restrictions. Approval prompts are explicit.",
  },
  {
    value: "auto",
    label: "Auto",
    description: "Balanced execution autonomy. Approval prompts are explicit.",
  },
  {
    value: "full",
    label: "Full",
    description: "Broadest execution autonomy. Approval prompts are explicit.",
  },
];

function status(mode: string): PolicyStatus {
  return {
    mode,
    alwaysApprove: ["shell"],
    autoApproveUnderUsd: null,
    approvalTtlHours: 24,
    manifestMode: mode,
    manifestAlwaysApprove: ["shell"],
    manifestAutoApproveUnderUsd: null,
    manifestApprovalTtlHours: null,
    overridden: false,
    takesEffect: "on the next turn",
    tiers: TIERS,
  };
}

/** A status carrying an operator override that differs from the manifest. */
function overridden(mode: string, manifestMode: string): PolicyStatus {
  return {
    ...status(mode),
    manifestMode,
    overridden: true,
    setBy: "someone",
  };
}

function makeClient(initial: PolicyStatus) {
  const put = vi.fn(async (_path: string, body: { mode?: string }) =>
    status(body.mode ?? initial.mode),
  );
  const del = vi.fn(async () => initial);
  return {
    client: {
      scopeFor: () => "/api/v1/acme",
      get: async (path: string) =>
        path.endsWith("/policy") ? initial : { slugs: [], unwired: [] },
      put,
      del,
    } as unknown as OpenCompanyClient,
    put,
    del,
  };
}

let container: HTMLDivElement;
let root: Root;

beforeEach(() => {
  (globalThis as unknown as { IS_REACT_ACT_ENVIRONMENT: boolean }).IS_REACT_ACT_ENVIRONMENT =
    true;
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
  vi.clearAllMocks();
});

afterEach(async () => {
  await act(async () => root.unmount());
  container.remove();
});

async function mount(client: OpenCompanyClient) {
  await act(async () => {
    root.render(createElement(PolicySettings, { client, company: "acme" }));
    await Promise.resolve();
  });
}

/** Types into a field the way an operator does, so React's state updates. */
async function type(box: HTMLInputElement, value: string) {
  await act(async () => {
    const setter = Object.getOwnPropertyDescriptor(
      window.HTMLInputElement.prototype,
      "value",
    )?.set;
    setter?.call(box, value);
    box.dispatchEvent(new Event("input", { bubbles: true }));
  });
}

describe("the autonomy direction", () => {
  it("uses the host's ordered list to identify widening moves", () => {
    expect(widensAutonomy(TIERS, "supervised", "full")).toBe(true);
    expect(widensAutonomy(TIERS, "full", "readonly")).toBe(false);
    expect(widensAutonomy(TIERS, "supervised", "unknown")).toBe(false);
    expect(widensAutonomy(TIERS, "unknown", "full")).toBe(false);
  });

  it("identifies spend-cap resets that widen the cap", () => {
    // `null` is the stricter state (every spend parks), so only moves toward a
    // finite cap — or a higher finite cap — are widening.
    expect(widensSpendCap(10, 20)).toBe(true);
    expect(widensSpendCap(10, null)).toBe(false);
    expect(widensSpendCap(null, 20)).toBe(true);
    expect(widensSpendCap(null, null)).toBe(false);
    expect(widensSpendCap(20, 10)).toBe(false);
    expect(widensSpendCap(10, 10)).toBe(false);
  });

  it("keeps the spend threshold inert while policy HITL is disabled", async () => {
    const { client, put } = makeClient(status("supervised"));
    await mount(client);
    const input = container.querySelector<HTMLInputElement>("#spend-cap")!;
    expect(input.disabled).toBe(true);
    expect(
      [...container.querySelectorAll("button")].find((button) =>
        button.textContent?.includes("Save cap"),
      )?.disabled,
    ).toBe(true);
    expect(put).not.toHaveBeenCalled();
    expect(container.textContent).toContain("Spend approval threshold (inactive)");
  });

  it("gates the same way the host matcher does", () => {
    expect(gatedBy(["shell"], "shell")).toBe(true);
    expect(gatedBy(["payment"], "payment.send")).toBe(true);
    expect(gatedBy(["payment.send"], "payment")).toBe(false);
    expect(gatedBy(["pay"], "payroll.export")).toBe(false);
    expect(gatedBy(["Shell"], "shell")).toBe(true);
    expect(gatedBy([], "shell")).toBe(false);
    expect(gatedBy([""], "shell")).toBe(false);
  });

  it("folds ASCII case only, like the host matcher", () => {
    // `"Ä".toLowerCase() === "ä"` in JS, but the host's `eq_ignore_ascii_case`
    // compares bytes, so a non-ASCII case pair is NOT a match — the
    // confirmation must not think a fence survives a reset when the gate does
    // not.
    expect(gatedBy(["ä"], "Ä")).toBe(false);
    expect(gatedBy(["shell"], "SHELL")).toBe(true);
  });

  it("shows the looser end of the scale in the console's amber risk tone", async () => {
    await mount(makeClient(status("supervised")).client);
    expect(container.querySelector("[data-testid=policy-tier-auto]")?.className).toContain(
      "status-blocked",
    );
    expect(container.querySelector("[data-testid=policy-tier-full]")?.className).toContain(
      "status-blocked",
    );
    expect(container.textContent).toContain("More autonomy");
  });
});

describe("changing the autonomy tier", () => {
  it("confirms a widening move with the before-and-after consequences", async () => {
    const { client, put } = makeClient(status("supervised"));
    await mount(client);

    await act(async () => {
      container.querySelector<HTMLButtonElement>("[data-testid=policy-tier-full]")!.click();
    });
    expect(put).not.toHaveBeenCalled();
    expect(document.body.textContent).toContain("Conservative execution restrictions");
    expect(document.body.textContent).toContain("Broadest execution autonomy");
    expect(document.body.textContent).toContain(
      "Approval prompts remain explicit through request_approval",
    );

    await act(async () => {
      document
        .querySelector<HTMLButtonElement>("[data-testid=policy-tier-confirm]")!
        .click();
      await Promise.resolve();
    });
    expect(put).toHaveBeenCalledWith("/api/v1/acme/policy", { mode: "full" });
  });

  it("keeps a narrowing move to one click", async () => {
    const { client, put } = makeClient(status("full"));
    await mount(client);

    await act(async () => {
      container
        .querySelector<HTMLButtonElement>("[data-testid=policy-tier-supervised]")!
        .click();
      await Promise.resolve();
    });
    expect(put).toHaveBeenCalledWith("/api/v1/acme/policy", { mode: "supervised" });
    expect(document.querySelector("[data-testid=policy-tier-confirm]")).toBeNull();
  });

  it("keeps the dialog up when Escape is pressed while the save is in flight", async () => {
    // The PUT stays unresolved, so `saving` is true while the dialog is open —
    // exactly the window where Base UI forwards an Escape close request.
    let resolvePut: (saved: PolicyStatus) => void = () => {};
    const put = vi.fn(
      async (_path: string) =>
        new Promise<PolicyStatus>((resolve) => {
          resolvePut = resolve;
        }),
    );
    const client = {
      scopeFor: () => "/api/v1/acme",
      get: async (path: string) =>
        path.endsWith("/policy") ? status("supervised") : { slugs: [], unwired: [] },
      put,
      del: async () => status("supervised"),
    } as unknown as OpenCompanyClient;
    await mount(client);

    await act(async () => {
      container.querySelector<HTMLButtonElement>("[data-testid=policy-tier-full]")!.click();
    });
    await act(async () => {
      document
        .querySelector<HTMLButtonElement>("[data-testid=policy-tier-confirm]")!
        .click();
      await Promise.resolve();
    });
    expect(put).toHaveBeenCalledWith("/api/v1/acme/policy", { mode: "full" });

    // Escape while the PUT is still in flight must not dismiss the dialog:
    // the request is still running and its outcome owns this screen.
    await act(async () => {
      document.dispatchEvent(
        new KeyboardEvent("keydown", { key: "Escape", bubbles: true }),
      );
      await Promise.resolve();
    });
    expect(document.querySelector("[data-testid=policy-tier-confirm]")).not.toBeNull();

    // The request resolving (here: succeeding) is what closes the dialog.
    await act(async () => {
      resolvePut(status("full"));
      await Promise.resolve();
    });
    expect(document.querySelector("[data-testid=policy-tier-confirm]")).toBeNull();
  });

  it("drops a pending confirmation when the company changes underneath it", async () => {
    const { client, put } = makeClient(status("supervised"));
    await mount(client);

    await act(async () => {
      container.querySelector<HTMLButtonElement>("[data-testid=policy-tier-full]")!.click();
    });
    expect(document.querySelector("[data-testid=policy-tier-confirm]")).not.toBeNull();

    // The scope moves to another company while the dialog is open: the pending
    // choice was reviewed against "acme"'s policy and must not apply to the
    // new one.
    await act(async () => {
      root.render(createElement(PolicySettings, { client, company: "other" }));
      await Promise.resolve();
    });
    expect(document.querySelector("[data-testid=policy-tier-confirm]")).toBeNull();
    expect(put).not.toHaveBeenCalled();
  });

  it("does not offer an always-ask save while policy HITL is disabled", async () => {
    const put = vi.fn();
    const client = {
      scopeFor: () => "/api/v1/acme",
      get: async (path: string) =>
        path.endsWith("/policy") ? status("supervised") : { slugs: [], unwired: [] },
      put,
      del: async () => status("supervised"),
    } as unknown as OpenCompanyClient;
    await mount(client);

    expect(container.querySelector<HTMLInputElement>("#always-approve")?.disabled).toBe(true);
    expect(put).not.toHaveBeenCalled();

    // The scope moves to another company while the PUT is in flight.
    await act(async () => {
      root.render(createElement(PolicySettings, { client, company: "other" }));
      await Promise.resolve();
    });

    expect(toasts.success).not.toHaveBeenCalled();
    expect(document.body.textContent).not.toContain("Always-ask list updated");
  });

  it("keeps explicit-approval copy on a widening confirmation", async () => {
    const { client } = makeClient(status("supervised"));
    await mount(client);

    await act(async () => {
      container.querySelector<HTMLButtonElement>("[data-testid=policy-tier-full]")!.click();
    });
    expect(document.body.textContent).toContain(
      "Approval prompts remain explicit through request_approval.",
    );
  });
});

describe("resetting to the manifest's policy", () => {
  it("confirms a reset that restores a more autonomous manifest tier", async () => {
    const { client, del } = makeClient(overridden("readonly", "full"));
    await mount(client);

    await act(async () => {
      const button = [...container.querySelectorAll("button")].find((b) =>
        b.textContent?.includes("manifest's policy"),
      )!;
      button.click();
    });
    expect(del).not.toHaveBeenCalled();
    expect(document.body.textContent).toContain(
      "Give teammates more autonomy?",
    );
    expect(document.body.textContent).toContain(
      "Broadest execution autonomy",
    );
    expect(document.body.textContent).toContain(
      "This also replaces the current always-ask list",
    );
    expect(document.body.textContent).toContain(
      "Reset restores the stored policy fields",
    );

    await act(async () => {
      document
        .querySelector<HTMLButtonElement>("[data-testid=policy-tier-confirm]")!
        .click();
      await Promise.resolve();
    });
    expect(del).toHaveBeenCalledWith("/api/v1/acme/policy");
  });

  it("restores focus to the checked tier after a successful reset", async () => {
    // The DELETE resolves to the manifest state — overridden=false — so the
    // reset button unmounts before the dialog closes, the exact moment the
    // `finalFocus` reset branch must fall back to the checked tier radio.
    const del = vi.fn(async () => status("full"));
    const client = {
      scopeFor: () => "/api/v1/acme",
      get: async (path: string) =>
        path.endsWith("/policy")
          ? overridden("readonly", "full")
          : { slugs: [], unwired: [] },
      put: vi.fn(async () => status("full")),
      del,
    } as unknown as OpenCompanyClient;
    await mount(client);

    await act(async () => {
      const button = [...container.querySelectorAll("button")].find((b) =>
        b.textContent?.includes("manifest's policy"),
      )!;
      button.click();
    });
    await act(async () => {
      document
        .querySelector<HTMLButtonElement>("[data-testid=policy-tier-confirm]")!
        .click();
      await Promise.resolve();
      await Promise.resolve();
    });
    expect(del).toHaveBeenCalledWith("/api/v1/acme/policy");
    // The override cleared, so the reset button is gone; focus must land on
    // the checked tier radio rather than falling out of the interface.
    expect(
      [...container.querySelectorAll("button")].some((b) =>
        b.textContent?.includes("manifest's policy"),
      ),
    ).toBe(false);
    expect(document.activeElement).toBe(
      document.querySelector("[data-testid=policy-tier-full]"),
    );
  });

  it("confirms a reset that drops an always-ask gate the manifest does not carry", async () => {
    const initial: PolicyStatus = {
      mode: "full",
      alwaysApprove: ["shell"],
      autoApproveUnderUsd: null,
      approvalTtlHours: 24,
      manifestMode: "full",
      manifestAlwaysApprove: [],
      manifestAutoApproveUnderUsd: null,
      manifestApprovalTtlHours: null,
      overridden: true,
      setBy: "someone",
      takesEffect: "on the next turn",
      tiers: TIERS,
    };
    const { client, del } = makeClient(initial);
    await mount(client);

    await act(async () => {
      const button = [...container.querySelectorAll("button")].find((b) =>
        b.textContent?.includes("manifest's policy"),
      )!;
      button.click();
    });
    expect(del).not.toHaveBeenCalled();
    expect(document.body.textContent).toContain(
      "Give teammates more autonomy?",
    );
    expect(document.body.textContent).not.toContain("Instead of:");
    expect(document.body.textContent).toContain(
      "This replaces the current always-ask list with the manifest's list: none",
    );
    expect(document.body.textContent).toContain(
      "shell stops always asking for approval",
    );

    await act(async () => {
      document
        .querySelector<HTMLButtonElement>("[data-testid=policy-tier-confirm]")!
        .click();
      await Promise.resolve();
    });
    expect(del).toHaveBeenCalledWith("/api/v1/acme/policy");
  });

  it("confirms a reset that loosens the manifest spend cap", async () => {
    const initial: PolicyStatus = {
      ...overridden("full", "full"),
      autoApproveUnderUsd: 10,
      manifestAutoApproveUnderUsd: 20,
    };
    const { client, del } = makeClient(initial);
    await mount(client);

    await act(async () => {
      [...container.querySelectorAll("button")]
        .find((button) => button.textContent?.includes("manifest's policy"))!
        .click();
    });
    expect(del).not.toHaveBeenCalled();
    expect(document.body.textContent).toContain(
      "restores the manifest's looser spend cap",
    );

    await act(async () => {
      document
        .querySelector<HTMLButtonElement>("[data-testid=policy-tier-confirm]")!
        .click();
      await Promise.resolve();
    });
    expect(del).toHaveBeenCalledWith("/api/v1/acme/policy");
  });
  it("keeps a reset that tightens the tier to one click", async () => {
    const { client, del } = makeClient(overridden("full", "readonly"));
    await mount(client);

    await act(async () => {
      const button = [...container.querySelectorAll("button")].find((b) =>
        b.textContent?.includes("manifest's policy"),
      )!;
      button.click();
      await Promise.resolve();
    });
    expect(del).toHaveBeenCalledWith("/api/v1/acme/policy");
    expect(document.querySelector("[data-testid=policy-tier-confirm]")).toBeNull();
  });

  it("names an immediate deadline when a reset lengthens the deadline", async () => {
    // The override shortened the deadline to 24h; the manifest names 72h. A
    // reset lands 72h on the live gate immediately — already-parked approvals
    // are judged against it on the next display or sweep — so the success
    // message must not fall back to the generic "next turn" line.
    const initial: PolicyStatus = {
      ...status("supervised"),
      approvalTtlHours: 24,
      manifestApprovalTtlHours: 72,
      overridden: true,
    };
    const del = vi.fn(async () => ({
      ...status("supervised"),
      approvalTtlHours: 72,
      manifestApprovalTtlHours: 72,
      overridden: false,
    }));
    const client = {
      scopeFor: () => "/api/v1/acme",
      get: async (path: string) =>
        path.endsWith("/policy") ? initial : { slugs: [], unwired: [] },
      put: vi.fn(async () => status("supervised")),
      del,
    } as unknown as OpenCompanyClient;
    await mount(client);

    await act(async () => {
      const button = [...container.querySelectorAll("button")].find((b) =>
        b.textContent?.includes("manifest's policy"),
      )!;
      button.click();
      await Promise.resolve();
    });
    expect(del).toHaveBeenCalledWith("/api/v1/acme/policy");
    expect(toasts.success).toHaveBeenCalledWith(
      "Reverted to the manifest's policy",
      expect.objectContaining({
        description:
          "takes effect immediately — parked approvals are re-checked against the manifest deadline",
      }),
    );
  });
});

describe("changing the spend cap", () => {
  it("does not offer a cap raise while policy HITL is disabled", async () => {
    const initial: PolicyStatus = { ...status("supervised"), autoApproveUnderUsd: 5 };
    const { client, put } = makeClient(initial);
    await mount(client);

    const input = container.querySelector<HTMLInputElement>("#spend-cap")!;
    expect(input.disabled).toBe(true);
    expect(put).not.toHaveBeenCalled();
    expect(document.body.textContent).toContain("Spend approval threshold (inactive)");
    expect(document.querySelector("[data-testid=policy-tier-confirm]")).toBeNull();
  });

  it("does not offer a tightening cap change while policy HITL is disabled", async () => {
    const initial: PolicyStatus = { ...status("supervised"), autoApproveUnderUsd: 100 };
    const { client, put } = makeClient(initial);
    await mount(client);

    const input = container.querySelector<HTMLInputElement>("#spend-cap")!;
    expect(input.disabled).toBe(true);
    expect(put).not.toHaveBeenCalled();
    expect(document.querySelector("[data-testid=policy-tier-confirm]")).toBeNull();
  });

  it("does not offer selecting no cap while policy HITL is disabled", async () => {
    const initial: PolicyStatus = { ...status("supervised"), autoApproveUnderUsd: 100 };
    const { client, put } = makeClient(initial);
    await mount(client);

    const input = container.querySelector<HTMLInputElement>("#spend-cap")!;
    expect(put).not.toHaveBeenCalled();
    expect(input.disabled).toBe(true);
    expect(
      [...container.querySelectorAll("button")].find((button) =>
        button.textContent?.includes("Set no cap"),
      )?.disabled,
    ).toBe(true);
  });

  it("keeps an unsaved always-ask edit when saving the deadline", async () => {
    const { client } = makeClient(status("supervised"));
    await mount(client);

    await type(
      container.querySelector<HTMLInputElement>("#always-approve")!,
      "shell, http_request",
    );
    const deadline = container.querySelector<HTMLInputElement>("#approval-deadline")!;
    await type(deadline, "48");
    await act(async () => {
      [...container.querySelectorAll("button")]
        .find((button) => button.textContent?.includes("Save deadline"))!
        .click();
      await Promise.resolve();
    });
    expect(container.querySelector<HTMLInputElement>("#always-approve")?.value).toBe(
      "shell, http_request",
    );
  });

  it("keeps an unsaved spend-cap edit when saving the deadline", async () => {
    const initial: PolicyStatus = { ...status("supervised"), autoApproveUnderUsd: 10 };
    const { client } = makeClient(initial);
    await mount(client);

    const spend = container.querySelector<HTMLInputElement>("#spend-cap")!;
    await type(spend, "25");
    const deadline = container.querySelector<HTMLInputElement>("#approval-deadline")!;
    await type(deadline, "48");
    await act(async () => {
      [...container.querySelectorAll("button")]
        .find((button) => button.textContent?.includes("Save deadline"))!
        .click();
      await Promise.resolve();
    });
    expect(container.querySelector<HTMLInputElement>("#spend-cap")?.value).toBe("25");
  });
});

describe("loading the policy", () => {
  it("clears the previous company's policy when a company-switch read fails", async () => {
    const { client } = makeClient(status("supervised"));
    await mount(client);
    expect(container.querySelector<HTMLInputElement>("#approval-deadline")?.value).toBe("24");

    // The scope moves to another company whose read fails. The card must show
    // the error, not "acme"'s policy as if it were the new company's — an
    // operator would otherwise save the old company's values against it.
    const failing = {
      ...client,
      scopeFor: () => "/api/v1/other",
      get: async () => {
        throw new Error("network down");
      },
    } as unknown as OpenCompanyClient;
    await act(async () => {
      root.render(createElement(PolicySettings, { client: failing, company: "other" }));
      await Promise.resolve();
      await Promise.resolve();
    });

    expect(container.textContent).toContain("network down");
    expect(container.querySelector<HTMLInputElement>("#approval-deadline")).toBeNull();
    expect(container.querySelector<HTMLInputElement>("#spend-cap")).toBeNull();
  });

  it("ignores a stale read that resolves after the company changed", async () => {
    // A `get` that parks every policy response until the test releases it, in
    // call order: the first render ("acme") holds resolver [0] and the
    // company-switch render ("other") holds [1].
    const held: Array<(value: PolicyStatus) => void> = [];
    const client = {
      scopeFor: () => "/api/v1/acme",
      get: (path: string) =>
        path.endsWith("/policy")
          ? new Promise<PolicyStatus>((resolve) => held.push(resolve))
          : Promise.resolve({ slugs: [], unwired: [] }),
      put: async () => status("readonly"),
      del: async () => status("readonly"),
    } as unknown as OpenCompanyClient;

    await act(async () => {
      root.render(createElement(PolicySettings, { client, company: "acme" }));
      await Promise.resolve();
    });
    // Move to another company while "acme"'s read is still in flight.
    await act(async () => {
      root.render(createElement(PolicySettings, { client, company: "other" }));
      await Promise.resolve();
    });

    // The response describing the visible company lands first and wins.
    await act(async () => {
      held[1]?.(status("full"));
      await Promise.resolve();
    });
    expect(
      container.querySelector<HTMLElement>("[data-testid=policy-tier-full]")?.getAttribute("aria-checked"),
    ).toBe("true");
    expect(container.querySelector<HTMLInputElement>("#approval-deadline")?.value).toBe("24");

    // The stale "acme" response resolving late must not overwrite it: the
    // visible company keeps its tier, and its draft deadline stays put.
    await act(async () => {
      held[0]?.(status("readonly"));
      await Promise.resolve();
    });
    expect(
      container.querySelector<HTMLElement>("[data-testid=policy-tier-full]")?.getAttribute("aria-checked"),
    ).toBe("true");
    expect(
      container.querySelector<HTMLElement>("[data-testid=policy-tier-readonly]")?.getAttribute("aria-checked"),
    ).toBe("false");
    expect(container.querySelector<HTMLInputElement>("#approval-deadline")?.value).toBe("24");
  });

  it("never starts an inactive cap save across a company switch", async () => {
    const heldGet: Array<(value: PolicyStatus) => void> = [];
    const acme = { ...status("supervised"), autoApproveUnderUsd: 10 };
    const put = vi.fn(async () => acme);
    const client = {
      scopeFor: () => "/api/v1/acme",
      get: (path: string) =>
        path.endsWith("/policy")
          ? new Promise<PolicyStatus>((resolve) => heldGet.push(resolve))
          : Promise.resolve({ slugs: [], unwired: [] }),
      put,
      del: async () => acme,
    } as unknown as OpenCompanyClient;

    await act(async () => {
      root.render(createElement(PolicySettings, { client, company: "acme" }));
      await Promise.resolve();
    });
    await act(async () => {
      heldGet[0]?.(acme);
      await Promise.resolve();
    });

    expect(container.querySelector<HTMLInputElement>("#spend-cap")?.disabled).toBe(true);
    expect(put).not.toHaveBeenCalled();

    // The operator switches companies while the save is still pending.
    await act(async () => {
      root.render(createElement(PolicySettings, { client, company: "other" }));
      await Promise.resolve();
    });

    // "other"'s load resolves first and paints its card — full tier, no cap.
    await act(async () => {
      heldGet[1]?.(status("full"));
      await Promise.resolve();
    });
    expect(
      container.querySelector<HTMLElement>("[data-testid=policy-tier-full]")?.getAttribute("aria-checked"),
    ).toBe("true");

    expect(
      container.querySelector<HTMLElement>("[data-testid=policy-tier-full]")?.getAttribute("aria-checked"),
    ).toBe("true");
    expect(
      container.querySelector<HTMLElement>("[data-testid=policy-tier-supervised]")?.getAttribute("aria-checked"),
    ).toBe("false");
    expect(container.querySelector<HTMLInputElement>("#spend-cap")?.value).toBe("");
    expect(put).not.toHaveBeenCalled();
  });

  it("discards a manual retry that resolves after the company changed", async () => {
    // The first load fails so the card offers a retry; the retry and the
    // company-switch load both park their `get`, and the switch load resolves
    // first so the late retry response is the stale one.
    const heldGet: Array<(value: PolicyStatus) => void> = [];
    let calls = 0;
    const client = {
      scopeFor: () => "/api/v1/acme",
      get: (path: string) => {
        if (!path.endsWith("/policy"))
          return Promise.resolve({ slugs: [], unwired: [] });
        calls += 1;
        return calls === 1
          ? Promise.reject(new Error("network down"))
          : new Promise<PolicyStatus>((resolve) => heldGet.push(resolve));
      },
      put: async () => status("readonly"),
      del: async () => status("readonly"),
    } as unknown as OpenCompanyClient;

    await act(async () => {
      root.render(createElement(PolicySettings, { client, company: "acme" }));
      await Promise.resolve();
      await Promise.resolve();
    });
    expect(container.textContent).toContain("network down");

    // Click Try again, then switch companies while the retry is in flight.
    await act(async () => {
      [...container.querySelectorAll("button")]
        .find((button) => button.textContent?.includes("Try again"))!
        .click();
    });
    await act(async () => {
      root.render(createElement(PolicySettings, { client, company: "other" }));
      await Promise.resolve();
    });

    // "other"'s load (the second parked `get`, after the rejecting mount and
    // the parked retry) resolves first and paints its card.
    await act(async () => {
      heldGet[1]?.(status("full"));
      await Promise.resolve();
    });
    expect(
      container.querySelector<HTMLElement>("[data-testid=policy-tier-full]")?.getAttribute("aria-checked"),
    ).toBe("true");

    // The stale retry from "acme" (the first parked `get`) resolves late; it
    // must not overwrite it.
    await act(async () => {
      heldGet[0]?.(status("readonly"));
      await Promise.resolve();
    });
    expect(
      container.querySelector<HTMLElement>("[data-testid=policy-tier-full]")?.getAttribute("aria-checked"),
    ).toBe("true");
    expect(
      container.querySelector<HTMLElement>("[data-testid=policy-tier-readonly]")?.getAttribute("aria-checked"),
    ).toBe("false");
  });
});
