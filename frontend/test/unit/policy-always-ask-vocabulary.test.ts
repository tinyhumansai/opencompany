// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { OpenCompanyClient } from "@/api/client";
import type { PolicyStatus } from "@/api/policy";
import {
  alwaysApproveGates,
  alwaysAskPlaceholder,
  isAutonomyEscalation,
} from "@/components/policy-settings";

/**
 * Issue #1226: the always-ask field's worked example.
 *
 * It used to be `payment.send, filing.submit, external.publish` — the three
 * strings issue #684 deleted from the shipped default *because they gate
 * nothing*: on the harness path an `always_approve` entry is matched against
 * the tool name, and none of those three names a tool. An operator following
 * the field's own suggestion got a fence that was not there, and a
 * "list updated" toast confirming it.
 *
 * So the assertions here are about which vocabulary the control puts in front
 * of an operator: the tools this deployment actually wired, and never the
 * retired trio. The suggestions must stay *suggestions* — the effect namespace
 * is deliberately open (`src/policy/always_approve.rs`), so a `datalist` and
 * not a `select`, and nothing may reject typed text.
 */

const toasts = vi.hoisted(() => ({
  base: vi.fn(),
  success: vi.fn(),
  error: vi.fn(),
  warning: vi.fn(),
  info: vi.fn(),
}));

vi.mock("sonner", () => {
  const toast = Object.assign(toasts.base, {
    success: toasts.success,
    error: toasts.error,
    warning: toasts.warning,
    info: toasts.info,
  });
  return { toast };
});

const { PolicySettings } = await import("@/components/policy-settings");

const WIRED = ["shell", "apply_patch", "git_operations", "web_fetch", "http_request"];

/**
 * The complete gateable registry the policy response carries (`knownTools`) —
 * the workflow set plus agent tools the gate matches that cannot be workflow
 * nodes (`publish_artifact`, the `hosting_*` family). The note must not call
 * any of these a mistake.
 */
const KNOWN_TOOLS = [
  ...WIRED,
  "curl",
  "csv_export",
  "image_info",
  "web_search",
  "publish_artifact",
  "hosting_launch_site",
  "hosting_add_domain",
  "hosting_set_env",
  "repo_publish",
  "workspace_write",
];

const STATUS: PolicyStatus = {
  mode: "auto",
  alwaysApprove: [],
  autoApproveUnderUsd: null,
  approvalTtlHours: 24,
  manifestMode: "auto",
  manifestAlwaysApprove: [],
  manifestAutoApproveUnderUsd: null,
  manifestApprovalTtlHours: null,
  overridden: false,
  takesEffect: "on the next turn",
  tiers: [
    { value: "auto", label: "Auto", description: "Works alone, stops before money." },
    { value: "full", label: "Full", description: "Acts without asking." },
  ],
  knownTools: KNOWN_TOOLS,
} as unknown as PolicyStatus;

/** A client serving the policy and, optionally, the wired tool slugs. */
function makeClient({
  slugs,
  status = STATUS,
  resetStatus = STATUS,
}: {
  slugs?: string[] | "unavailable";
  status?: PolicyStatus;
  resetStatus?: PolicyStatus;
} = {}) {
  const put = vi.fn(async () => status);
  const del = vi.fn(async () => resetStatus);
  return {
    scopeFor: (company: string | null) => `/api/v1/${company ?? "company"}`,
    get: async (path: string) => {
      if (path.includes("/workflows/tool-slugs")) {
        if (slugs === "unavailable") throw new Error("this host predates the route");
        return { slugs: slugs ?? WIRED, unwired: [] };
      }
      if (path.endsWith("/policy")) return status;
      return null;
    },
    put,
    del,
  } as unknown as OpenCompanyClient;
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
  await act(async () => {
    root.unmount();
  });
  container.remove();
});

async function mount(client: OpenCompanyClient) {
  await act(async () => {
    root.render(createElement(PolicySettings, { client, company: "acme", canManage: true }));
  });
}

const field = () => container.querySelector<HTMLInputElement>("#always-approve");
const options = () =>
  Array.from(container.querySelectorAll<HTMLOptionElement>("datalist option")).map(
    (o) => o.value,
  );

describe("what the always-ask field suggests", () => {
  it("never offers the three kinds #684 removed for gating nothing", async () => {
    await mount(makeClient());
    const text = container.textContent ?? "";
    const shown = `${field()?.placeholder ?? ""} ${options().join(" ")} ${text}`;
    for (const retired of ["payment.send", "filing.submit", "external.publish"]) {
      expect(shown, `still recommends ${retired}`).not.toContain(retired);
    }
  });

  it("grounds its example on the tools this deployment wired", async () => {
    await mount(makeClient());
    expect(options()).toEqual(WIRED);
    // The placeholder is drawn from the same set, so the one example an
    // operator reads without opening anything is also a working entry.
    const placeholder = field()?.placeholder ?? "";
    for (const suggested of placeholder.split(", ")) {
      expect(WIRED).toContain(suggested);
    }
  });

  it("picks examples worth gating, not merely the first three wired", () => {
    // The host's own order put `read_workspace_state` — a read — into the
    // worked example. A valid entry and a pointless suggestion.
    expect(alwaysAskPlaceholder(["read_workspace_state", "shell", "http_request"])).toBe(
      "shell, http_request, read_workspace_state",
    );
    // A deployment wiring nothing consequential still gets its own tools back,
    // in the host's order, rather than a name it does not have.
    expect(alwaysAskPlaceholder(["image_info", "csv_export"])).toBe(
      "image_info, csv_export",
    );
    expect(alwaysAskPlaceholder([])).toBe("shell, http_request, publish_artifact");
  });

  it("leaves the box free text — the effect namespace is open on purpose", async () => {
    await mount(makeClient());
    const input = field()!;
    // A `datalist`, never a `select`: a hosted brain may emit a kind this
    // repository has never seen, and the host deliberately does not validate.
    expect(input.tagName).toBe("INPUT");
    expect(input.getAttribute("list")).toBe("always-approve-tools");
    await act(async () => {
      input.value = "some.custom.kind";
      input.dispatchEvent(new Event("input", { bubbles: true }));
    });
    expect(field()?.value).toBe("some.custom.kind");
  });

  it("degrades to a plain box when the host cannot serve the tool set", async () => {
    await mount(makeClient({ slugs: "unavailable" }));
    expect(options()).toEqual([]);
    expect(field()?.getAttribute("list")).toBeNull();
    // Still a working example rather than a retired one.
    expect(field()?.placeholder).toBe("shell, http_request, publish_artifact");
    // And a failed suggestions read must not report the policy card as broken.
    expect(toasts.error).not.toHaveBeenCalled();
  });

  it("does not call an entry unwired before tool discovery has answered", async () => {
    let resolveSlugs!: (r: { slugs: string[] }) => void;
    const client = {
      scopeFor: (company: string | null) => `/api/v1/${company ?? "company"}`,
      get: async (path: string) => {
        if (path.includes("/workflows/tool-slugs")) {
          // Held until the test resolves it, so the component renders with the
          // wired set still pending — an empty array, but not a proven one.
          return await new Promise((resolve) => {
            resolveSlugs = resolve;
          });
        }
        if (path.endsWith("/policy")) {
          // A host that does not serve the complete registry (`knownTools`):
          // the note has only the workflow set to fall back on, and an empty
          // set while discovery is pending proves nothing.
          return {
            ...STATUS,
            alwaysApprove: ["shell"],
            knownTools: undefined,
          };
        }
        return null;
      },
      put: vi.fn(async () => STATUS),
      del: vi.fn(async () => STATUS),
    } as unknown as OpenCompanyClient;

    await mount(client);
    // The always-ask list is nonempty, but an empty `wiredTools` while the
    // request is pending proves nothing — no "not a tool" claim yet.
    expect(container.textContent).not.toContain("is not a tool");
    expect(container.textContent).not.toContain("match any");

    await act(async () => {
      resolveSlugs({ slugs: [] });
    });
    // Discovery succeeded and "shell" is genuinely not a workflow tool here:
    // now it speaks — scoped to what the served set can prove, not a blanket
    // "not a tool" claim.
    expect(container.textContent).toContain(
      "shell doesn't match any of the workflow tools wired here.",
    );
  });

  it("withholds the unwired warning when the host cannot serve the tool set", async () => {
    const client = makeClient({
      slugs: "unavailable",
      status: {
        ...STATUS,
        alwaysApprove: ["shell"],
        // No complete registry either: neither source can prove what is wired,
        // so the note stays silent rather than guessing.
        knownTools: undefined,
      },
    });
    await mount(client);
    await act(async () => {
      await Promise.resolve();
      await Promise.resolve();
    });
    // An older host never answers discovery, so an entry cannot be proven
    // unwired — it may still be a hosted effect kind.
    expect(container.textContent).not.toContain("is not a tool");
    expect(container.textContent).not.toContain("match any");
  });

  it("marks the legacy field inactive while policy HITL is disabled", async () => {
    await mount(makeClient());
    const text = container.textContent ?? "";
    expect(text).toContain("Always ask first (inactive)");
    expect(text).toContain("do not create prompts now");
    expect(text).toContain("request_approval");
    expect(field()?.disabled).toBe(true);
  });

  it("treats a case-variant of a wired tool as wired, not a mistake", async () => {
    const client = makeClient();
    await mount(client);
    await act(async () => {
      await Promise.resolve();
      await Promise.resolve();
      await Promise.resolve();
    });
    const input = field()!;
    await act(async () => {
      const setValue = Object.getOwnPropertyDescriptor(
        HTMLInputElement.prototype,
        "value",
      )?.set;
      // The backend's matcher is case-insensitive (`src/policy/always_approve.rs`),
      // so `SHELL` gates the wired `shell` tool — the warning must not call it
      // a fence that is not there.
      setValue?.call(input, "SHELL");
      input.dispatchEvent(new Event("input", { bubbles: true }));
    });
    expect(container.textContent).not.toContain("match any");
  });

  it("does not call a real gateable agent tool a mistake", async () => {
    // `publish_artifact` is a tool the approval gate covers (it parks), but it
    // is not one of the workflow-authorable slugs served by
    // `/workflows/tool-slugs`. The complete registry (`knownTools`) carries it,
    // so the note must not flag it at all — the gate would match it by name.
    const client = makeClient();
    await mount(client);
    await act(async () => {
      await Promise.resolve();
      await Promise.resolve();
      await Promise.resolve();
    });
    const input = field()!;
    await act(async () => {
      const setValue = Object.getOwnPropertyDescriptor(
        HTMLInputElement.prototype,
        "value",
      )?.set;
      setValue?.call(input, "publish_artifact");
      input.dispatchEvent(new Event("input", { bubbles: true }));
    });
    expect(container.textContent).not.toContain("match any");
  });

  it("flags a typo against the complete registry, not just the workflow set", async () => {
    // `shel` gates nothing in `knownTools` (a case-insensitive, segment-bound
    // match), so the note speaks — with the confident wording the full registry
    // earns.
    const client = makeClient();
    await mount(client);
    await act(async () => {
      await Promise.resolve();
      await Promise.resolve();
      await Promise.resolve();
    });
    const input = field()!;
    await act(async () => {
      const setValue = Object.getOwnPropertyDescriptor(
        HTMLInputElement.prototype,
        "value",
      )?.set;
      setValue?.call(input, "shel");
      input.dispatchEvent(new Event("input", { bubbles: true }));
    });
    expect(container.textContent).toContain(
      "shel doesn't match any tool the approval gate recognizes.",
    );
    expect(container.textContent).toContain("It may still be a hosted effect kind.");
  });

  it("still hedges an unmatched dotted kind as possibly hosted", async () => {
    // `invoice.send` matches no declared tool, but the effect namespace is open
    // on purpose — the note says so rather than calling it a mistake.
    const client = makeClient();
    await mount(client);
    await act(async () => {
      await Promise.resolve();
      await Promise.resolve();
      await Promise.resolve();
    });
    const input = field()!;
    await act(async () => {
      const setValue = Object.getOwnPropertyDescriptor(
        HTMLInputElement.prototype,
        "value",
      )?.set;
      setValue?.call(input, "invoice.send");
      input.dispatchEvent(new Event("input", { bubbles: true }));
    });
    expect(container.textContent).toContain(
      "invoice.send doesn't match any tool the approval gate recognizes.",
    );
    expect(container.textContent).toContain("It may still be a hosted effect kind.");
  });
});

describe("policy tier changes", () => {
  it("identifies only a move to a higher host-ordered tier as an escalation", () => {
    expect(isAutonomyEscalation(STATUS.tiers, "auto", "full")).toBe(true);
    expect(isAutonomyEscalation(STATUS.tiers, "full", "auto")).toBe(false);
    expect(isAutonomyEscalation(STATUS.tiers, "auto", "auto")).toBe(false);
  });

  it("uses a labelled radio group and explains the autonomy axis", async () => {
    await mount(makeClient());
    const group = container.querySelector('[role="radiogroup"]');
    expect(group?.getAttribute("aria-labelledby")).toBe("approvals-heading");
    expect(group?.textContent).toContain("More oversight");
    expect(group?.textContent).toContain("More autonomy");

    const radios = Array.from(group?.querySelectorAll('[role="radio"]') ?? []);
    expect(radios).toHaveLength(2);
    expect(radios.map((radio) => radio.getAttribute("aria-checked"))).toEqual([
      "true",
      "false",
    ]);
  });

  it("confirms an autonomy escalation before persisting it", async () => {
    const client = makeClient();
    await mount(client);
    const full = Array.from(container.querySelectorAll<HTMLButtonElement>('[role="radio"]')).find(
      (button) => button.textContent?.includes("Full"),
    );

    await act(async () => {
      full?.click();
    });
    expect((client.put as ReturnType<typeof vi.fn>)).not.toHaveBeenCalled();
    expect(document.body.textContent).toContain("Give teammates more autonomy?");
    expect(document.body.textContent).toContain("Acts without asking.");

    const confirm = Array.from(document.body.querySelectorAll<HTMLButtonElement>("button")).find(
      (button) => button.textContent === "Give more autonomy",
    );
    await act(async () => {
      confirm?.click();
    });
    expect((client.put as ReturnType<typeof vi.fn>)).toHaveBeenCalledWith(
      "/api/v1/acme/policy",
      { mode: "full" },
    );
  });

  it("keeps tightening one click and softly flags unwired tool names", async () => {
    const client = makeClient({ status: { ...STATUS, mode: "full" } });
    await mount(client);
    await act(async () => {
      await Promise.resolve();
      await Promise.resolve();
      await Promise.resolve();
    });
    expect(options()).toEqual(WIRED);

    const input = field()!;
    await act(async () => {
      const setValue = Object.getOwnPropertyDescriptor(
        HTMLInputElement.prototype,
        "value",
      )?.set;
      setValue?.call(input, "shel, invoice.send");
      input.dispatchEvent(new Event("input", { bubbles: true }));
    });
    expect(container.textContent).toContain(
      "shel, invoice.send don't match any tool the approval gate recognizes.",
    );
    expect(container.textContent).toContain(
      "They may still be hosted effect kinds.",
    );

    const auto = Array.from(container.querySelectorAll<HTMLButtonElement>('[role="radio"]')).find(
      (button) => button.textContent?.includes("Auto"),
    );
    await act(async () => {
      auto?.click();
    });
    expect((client.put as ReturnType<typeof vi.fn>)).toHaveBeenCalledWith(
      "/api/v1/acme/policy",
      { mode: "auto" },
    );
  });

  it("roves the tab order and moves among tiers with the arrow keys", async () => {
    const client = makeClient();
    await mount(client);
    const radios = Array.from(
      container.querySelectorAll<HTMLButtonElement>('[role="radio"]'),
    );
    const auto = radios.find((button) => button.textContent?.includes("Auto"))!;
    const full = radios.find((button) => button.textContent?.includes("Full"))!;
    // Only the checked tier is in the Tab order.
    expect(auto.tabIndex).toBe(0);
    expect(full.tabIndex).toBe(-1);

    // Arrow-down onto Full is an escalation: focus moves, but the change waits
    // on the confirmation instead of persisting straight away.
    auto.focus();
    await act(async () => {
      auto.dispatchEvent(
        new KeyboardEvent("keydown", { key: "ArrowDown", bubbles: true }),
      );
    });
    expect(document.body.textContent).toContain("Give teammates more autonomy?");
    expect((client.put as ReturnType<typeof vi.fn>)).not.toHaveBeenCalled();
  });

  it("lets the arrow keys select a lower tier directly", async () => {
    const client = makeClient({ status: { ...STATUS, mode: "full" } });
    await mount(client);
    const full = Array.from(
      container.querySelectorAll<HTMLButtonElement>('[role="radio"]'),
    ).find((button) => button.textContent?.includes("Full"))!;
    full.focus();
    await act(async () => {
      full.dispatchEvent(
        new KeyboardEvent("keydown", { key: "ArrowUp", bubbles: true }),
      );
    });
    expect((client.put as ReturnType<typeof vi.fn>)).toHaveBeenCalledWith(
      "/api/v1/acme/policy",
      { mode: "auto" },
    );
  });

  it("navigates from the focused radio, not the selected tier", async () => {
    const THREE_TIERS: PolicyStatus = {
      ...STATUS,
      mode: "auto",
      tiers: [
        {
          value: "supervised",
          label: "Supervised",
          description: "Asks before every change.",
        },
        {
          value: "auto",
          label: "Auto",
          description: "Works alone, stops before money.",
        },
        {
          value: "full",
          label: "Full",
          description: "Acts without asking.",
        },
      ],
    };
    const client = makeClient({ status: THREE_TIERS });
    await mount(client);
    const radios = Array.from(
      container.querySelectorAll<HTMLButtonElement>('[role="radio"]'),
    );
    const full = radios.find((button) =>
      button.textContent?.includes("Full"),
    )!;
    const auto = radios.find((button) =>
      button.textContent?.includes("Auto"),
    )!;
    // Focus on Full while Auto is still selected — the state a cancelled
    // escalation leaves behind. ArrowUp must move to Auto, Full's own
    // neighbour, not to Supervised, which is Auto's neighbour. Auto is
    // already selected, so nothing persists; the point is where focus (and
    // the next key's arithmetic) land.
    full.focus();
    await act(async () => {
      full.dispatchEvent(
        new KeyboardEvent("keydown", { key: "ArrowUp", bubbles: true }),
      );
    });
    expect(document.activeElement).toBe(auto);
    expect((client.put as ReturnType<typeof vi.fn>)).not.toHaveBeenCalled();
  });

  it("wraps ArrowUp from the first tier to the last", async () => {
    const client = makeClient();
    await mount(client);
    const radios = Array.from(
      container.querySelectorAll<HTMLButtonElement>('[role="radio"]'),
    );
    const auto = radios.find((button) =>
      button.textContent?.includes("Auto"),
    )!;
    // Auto is the first tier and selected. ArrowUp has no neighbour above it,
    // so the group wraps to Full — an escalation, which parks in the
    // confirmation dialog instead of persisting. The dialog's focus trap owns
    // focus once it opens, so the wrap target is read off what the dialog says
    // (Full's description) rather than `document.activeElement`.
    auto.focus();
    await act(async () => {
      auto.dispatchEvent(
        new KeyboardEvent("keydown", { key: "ArrowUp", bubbles: true }),
      );
    });
    expect(document.body.textContent).toContain(
      "Give teammates more autonomy?",
    );
    expect(document.body.textContent).toContain("Acts without asking.");
    expect((client.put as ReturnType<typeof vi.fn>)).not.toHaveBeenCalled();
  });

  it("wraps ArrowDown from the last tier to the first", async () => {
    const client = makeClient({ status: { ...STATUS, mode: "full" } });
    await mount(client);
    const radios = Array.from(
      container.querySelectorAll<HTMLButtonElement>('[role="radio"]'),
    );
    const auto = radios.find((button) =>
      button.textContent?.includes("Auto"),
    )!;
    const full = radios.find((button) =>
      button.textContent?.includes("Full"),
    )!;
    // Full is the last tier and selected. ArrowDown has no neighbour below it,
    // so the group wraps to Auto — a downgrade, which lands immediately.
    full.focus();
    await act(async () => {
      full.dispatchEvent(
        new KeyboardEvent("keydown", { key: "ArrowDown", bubbles: true }),
      );
    });
    expect(document.activeElement).toBe(auto);
    expect((client.put as ReturnType<typeof vi.fn>)).toHaveBeenCalledWith(
      "/api/v1/acme/policy",
      { mode: "auto" },
    );
  });

  it("keeps the escalation confirmation open when saving fails", async () => {
    const failingPut = vi.fn(async () => {
      throw new Error("host refused");
    });
    const client = {
      scopeFor: (company: string | null) => `/api/v1/${company ?? "company"}`,
      get: async (path: string) => {
        if (path.includes("/workflows/tool-slugs")) {
          return { slugs: WIRED, unwired: [] };
        }
        if (path.endsWith("/policy")) return STATUS;
        return null;
      },
      put: failingPut,
      del: vi.fn(async () => STATUS),
    } as unknown as OpenCompanyClient;

    await mount(client);
    const full = Array.from(
      container.querySelectorAll<HTMLButtonElement>('[role="radio"]'),
    ).find((button) => button.textContent?.includes("Full"));
    await act(async () => {
      full?.click();
    });
    expect(document.body.textContent).toContain("Give teammates more autonomy?");

    const confirm = Array.from(
      document.body.querySelectorAll<HTMLButtonElement>("button"),
    ).find((button) => button.textContent === "Give more autonomy");
    await act(async () => {
      confirm?.click();
    });
    // A rejected PUT must not dismiss the dialog: the operator retries without
    // re-selecting the tier.
    expect(failingPut).toHaveBeenCalledWith("/api/v1/acme/policy", {
      mode: "full",
    });
    expect(document.body.textContent).toContain("Give teammates more autonomy?");
    expect(toasts.error).toHaveBeenCalled();
  });

  it("disables every tier control for a member and states why", async () => {
    const client = makeClient();
    await act(async () => {
      root.render(
        createElement(PolicySettings, { client, company: "acme", canManage: false }),
      );
    });

    expect(container.textContent).toContain(
      "Only an admin can change this company's approval policy",
    );

    const radios = Array.from(
      container.querySelectorAll<HTMLButtonElement>('[role="radio"]'),
    );
    expect(radios.length).toBeGreaterThan(0);
    expect(radios.every((radio) => radio.disabled)).toBe(true);

    const full = radios.find((radio) => radio.textContent?.includes("Full"));
    await act(async () => {
      full?.click();
    });
    expect(document.body.textContent).not.toContain("Give teammates more autonomy?");
    expect((client.put as ReturnType<typeof vi.fn>)).not.toHaveBeenCalled();
  });

  it("withholds the deadline save control from a member", async () => {
    const client = makeClient();
    await act(async () => {
      root.render(
        createElement(PolicySettings, { client, company: "acme", canManage: false }),
      );
    });

    const buttons = Array.from(container.querySelectorAll("button")).map(
      (button) => button.textContent,
    );
    expect(buttons).not.toContain("Save deadline");
    expect(
      container.querySelector<HTMLInputElement>("#approval-deadline")?.disabled,
    ).toBe(true);
  });
});

describe("alwaysApproveGates", () => {
  it("matches exactly, case-insensitively, like the backend matcher", () => {
    expect(alwaysApproveGates("SHELL", "shell")).toBe(true);
    expect(
      alwaysApproveGates("  Publish_Artifact ", "publish_artifact"),
    ).toBe(true);
    expect(alwaysApproveGates("shell", "http_request")).toBe(false);
  });

  it("gates a leading dotted segment", () => {
    expect(alwaysApproveGates("invoice", "invoice.send")).toBe(true);
    expect(alwaysApproveGates("invoice", "invoice.refund")).toBe(true);
    expect(alwaysApproveGates("Invoice", "invoice.send")).toBe(true);
  });

  it("keeps the segment boundary load-bearing", () => {
    expect(alwaysApproveGates("pay", "payroll.export")).toBe(false);
    expect(alwaysApproveGates("payment", "payments_report")).toBe(false);
  });

  it("folds ASCII case only, like eq_ignore_ascii_case", () => {
    // The Kelvin sign is a Unicode case confusable: full-Unicode lowercasing
    // turns `worKspace_write` into `workspace_write`, but the backend's
    // ASCII-only `eq_ignore_ascii_case` does not fold U+212A, so this fence
    // would never gate — the warning must not accept it either.
    expect(alwaysApproveGates("worKspace_write", "workspace_write")).toBe(
      false,
    );
    expect(alwaysApproveGates("WORKSPACE_WRITE", "workspace_write")).toBe(
      false,
    );
    expect(alwaysApproveGates("SHELL", "shell")).toBe(true);
    expect(alwaysApproveGates("Invoice", "invoice.send")).toBe(true);
  });

  it("treats an empty entry as gating nothing", () => {
    expect(alwaysApproveGates("  ", "shell")).toBe(false);
    expect(alwaysApproveGates("", "shell")).toBe(false);
  });
});

describe("manifest resets", () => {
  it("confirms a reset that would escalate to the manifest's tier", async () => {
    const client = makeClient({
      status: {
        ...STATUS,
        mode: "auto",
        manifestMode: "full",
        overridden: true,
        setBy: "alice@acme",
      },
      resetStatus: {
        ...STATUS,
        mode: "full",
        manifestMode: "full",
        overridden: false,
      },
    });
    await mount(client);
    const resetButton = Array.from(
      container.querySelectorAll<HTMLButtonElement>("button"),
    ).find((button) =>
      button.textContent?.includes("Use the manifest's policy"),
    );
    expect(resetButton).toBeDefined();

    await act(async () => {
      resetButton?.click();
    });
    // Nothing persisted yet; the escalation confirmation is up instead.
    expect((client.del as ReturnType<typeof vi.fn>)).not.toHaveBeenCalled();
    expect(document.body.textContent).toContain("Give teammates more autonomy?");
    expect(document.body.textContent).toContain("manifest's Full setting");

    const confirm = Array.from(
      document.body.querySelectorAll<HTMLButtonElement>("button"),
    ).find((button) => button.textContent === "Revert and give more autonomy");
    await act(async () => {
      confirm?.click();
    });
    expect((client.del as ReturnType<typeof vi.fn>)).toHaveBeenCalledWith(
      "/api/v1/acme/policy",
    );
  });

  it("resets immediately when the manifest's tier is not higher", async () => {
    const client = makeClient({
      status: {
        ...STATUS,
        mode: "full",
        manifestMode: "auto",
        overridden: true,
      },
      resetStatus: {
        ...STATUS,
        mode: "auto",
        manifestMode: "auto",
        overridden: false,
      },
    });
    await mount(client);
    const resetButton = Array.from(
      container.querySelectorAll<HTMLButtonElement>("button"),
    ).find((button) =>
      button.textContent?.includes("Use the manifest's policy"),
    );
    await act(async () => {
      resetButton?.click();
    });
    expect((client.del as ReturnType<typeof vi.fn>)).toHaveBeenCalledWith(
      "/api/v1/acme/policy",
    );
    expect(document.body.textContent).not.toContain(
      "Give teammates more autonomy?",
    );
  });
});
