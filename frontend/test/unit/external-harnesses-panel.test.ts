// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { OpenCompanyClient } from "@/api/client";
import { ApiError, type HarnessDto } from "@/api/types";
import { ExternalHarnesses } from "@/components/external-harnesses";

/**
 * The External harnesses panel, after the probe half moved into
 * `useHarnessRows` so the agent picker could share it.
 *
 * The panel kept two things the hook does not have: its own
 * `GET {scope}/harnesses` fetch, and the 404 → render-nothing branch for a
 * host predating that route. Both had recorded bugs and neither had a test,
 * which is what made the move worth pinning down here.
 */

let container: HTMLDivElement;
let root: Root;

function harness(over: Partial<HarnessDto> = {}): HarnessDto {
  return { id: "claude", kind: "acp", default: false, detected: true, ...over };
}

function fakeClient(listHarnesses: () => Promise<HarnessDto[]>): OpenCompanyClient {
  return { listHarnesses } as unknown as OpenCompanyClient;
}

async function show(client: OpenCompanyClient, company: string | null = "acme") {
  await act(async () => {
    root.render(createElement(ExternalHarnesses, { client, company }));
  });
  await act(async () => {});
}

function rows(): HTMLElement[] {
  return Array.from(container.querySelectorAll('[data-testid="harness-row"]'));
}

function checkAgain(): HTMLButtonElement {
  const found = Array.from(container.querySelectorAll("button")).find((b) =>
    b.textContent?.includes("Check again"),
  );
  if (!found) throw new Error(`no "Check again" button; saw: ${container.innerHTML}`);
  return found as HTMLButtonElement;
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
  vi.restoreAllMocks();
});

describe("External harnesses panel", () => {
  it("renders a row per declared harness, said in words rather than only in hue", async () => {
    await show(
      fakeClient(async () => [
        harness({ id: "claude", default: true }),
        harness({ id: "inhouse", kind: "built_in" }),
      ]),
    );

    expect(rows()).toHaveLength(2);
    // A browser cannot see a local CLI, so the ACP row says which — not
    // "Not installed", which would send someone to reinstall what they have.
    expect(container.textContent).toContain("Desktop only");
    expect(container.textContent).toContain("Managed");
    expect(container.textContent).toContain("Default");
  });

  it("renders nothing at all against a host that has no harness route", async () => {
    await show(
      fakeClient(async () => {
        throw new ApiError(404, "not_found", "no such route");
      }),
    );

    expect(container.innerHTML).toBe("");
  });

  it("does not let a superseded 404 hide a panel that is working", async () => {
    // Settings is reused across company changes. The first fetch is an older
    // host's 404 and is deliberately still in flight when the second company's
    // fetch lands — the shape that once blanked a working panel.
    let rejectFirst: ((error: unknown) => void) | null = null;
    const listHarnesses = vi
      .fn<() => Promise<HarnessDto[]>>()
      .mockImplementationOnce(
        () =>
          new Promise<HarnessDto[]>((_resolve, reject) => {
            rejectFirst = reject;
          }),
      )
      .mockImplementationOnce(async () => [harness({ id: "codex" })]);
    const client = fakeClient(listHarnesses);

    await show(client, "first");
    await show(client, "second");
    expect(rows()).toHaveLength(1);

    await act(async () => {
      rejectFirst?.(new ApiError(404, "not_found", "no such route"));
      await Promise.resolve();
    });

    expect(
      container.innerHTML,
      "a 404 from the superseded company must not blank the current one's rows",
    ).not.toBe("");
    expect(rows()).toHaveLength(1);
  });

  it("holds Check again until the rows it is fetching are on screen", async () => {
    let settle: ((list: HarnessDto[]) => void) | null = null;
    const client = fakeClient(
      () =>
        new Promise<HarnessDto[]>((resolve) => {
          settle = resolve;
        }),
    );

    await act(async () => {
      root.render(createElement(ExternalHarnesses, { client, company: "acme" }));
    });
    expect(checkAgain().disabled).toBe(true);

    await act(async () => {
      settle?.([harness({ id: "claude" })]);
      await Promise.resolve();
    });
    await act(async () => {});

    // Re-armed only once the survey has produced rows — never in the window
    // between the fetch landing and the join, where the pane has nothing to
    // show and would read as an empty list.
    expect(checkAgain().disabled).toBe(false);
    expect(rows()).toHaveLength(1);
  });
});
