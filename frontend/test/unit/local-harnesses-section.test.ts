// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { OpenCompanyClient } from "@/api/client";
import { ApiError, type HarnessDto, type TeamMemberDto } from "@/api/types";
import { LocalHarnesses } from "@/inference/LocalHarnesses";

/**
 * The Local harnesses section on the LLM Providers page.
 *
 * Three claims are pinned here, because each one was a way to get this wrong:
 * the section is absent rather than empty when there is nothing local (a hosted
 * build declares none), a row counts teammates the way the host's own lane rule
 * does, and it says "bound" rather than borrowing the providers list's
 * "connected" for something no company holds.
 */

let container: HTMLDivElement;
let root: Root;

function harness(over: Partial<HarnessDto> = {}): HarnessDto {
  return { id: "claude", kind: "acp", default: false, detected: true, ...over };
}

function member(id: string, harness?: string): TeamMemberDto {
  const base = { id, role: "Writer", name: id } as TeamMemberDto;
  return harness === undefined ? base : { ...base, harness };
}

function fakeClient(
  listHarnesses: () => Promise<HarnessDto[]>,
  roster: TeamMemberDto[] = [],
): OpenCompanyClient {
  return { listHarnesses, listTeam: async () => roster } as unknown as OpenCompanyClient;
}

async function show(client: OpenCompanyClient) {
  await act(async () => {
    root.render(createElement(LocalHarnesses, { client, company: "acme" }));
  });
  await act(async () => {});
}

function section(): HTMLElement | null {
  return container.querySelector('[data-testid="inference-local-harnesses"]');
}

function rows(): HTMLElement[] {
  return Array.from(container.querySelectorAll('[data-testid="local-harness-row"]'));
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

describe("Local harnesses section", () => {
  it("renders nothing at all when no local harness is declared", async () => {
    // What a hosted build produces: `can_run_local_acp()` gates it upstream, so
    // the list simply arrives without them. An empty heading would be a section
    // promising a kind of row this deployment can never have.
    await show(fakeClient(async () => [harness({ id: "inhouse", kind: "built_in", default: true })]));
    expect(section()).toBeNull();
    expect(container.textContent).not.toContain("Local harnesses");
  });

  it("renders nothing when the company declares no harnesses at all", async () => {
    await show(fakeClient(async () => []));
    expect(section()).toBeNull();
  });

  it("renders nothing on a host predating the harnesses route", async () => {
    await show(
      fakeClient(async () => {
        throw new ApiError(404, "not_found", "no such route");
      }),
    );
    expect(section()).toBeNull();
  });

  it("lists a row per local harness and never says connected", async () => {
    await show(
      fakeClient(async () => [
        harness({ id: "claude" }),
        harness({ id: "inhouse", kind: "built_in", default: true }),
      ]),
    );
    expect(rows()).toHaveLength(1);
    expect(section()!.textContent).toContain("claude");
    // The built-in harness is not a local CLI and has no row here.
    expect(rows()[0].textContent).not.toContain("inhouse");
    expect(section()!.textContent).not.toMatch(/connected/i);
  });

  it("counts teammates as bound, resolving an undeclared one to the default", async () => {
    await show(
      fakeClient(
        async () => [
          harness({ id: "inhouse", kind: "built_in", default: true }),
          harness({ id: "claude" }),
        ],
        [member("ceo"), member("writer", "claude"), member("jamie", "claude")],
      ),
    );
    const bound = container.querySelector('[data-testid="local-harness-bound"]')!;
    // `ceo` declares nothing, so it is on `inhouse` — the default — and not here.
    expect(bound.textContent).toBe("2 agents bound");
  });

  it("still renders when the roster cannot be read, claiming no count", async () => {
    // The section hangs off the Providers page, which has hosts that answer the
    // harness list and not this. A failed count must cost the count, not the
    // page — and silence is the honest rendering of it, since zero is a claim.
    const client = {
      listHarnesses: async () => [harness({ id: "claude", default: true })],
    } as unknown as OpenCompanyClient;
    await show(client);
    expect(section()).not.toBeNull();
    expect(container.querySelector('[data-testid="local-harness-bound"]')).toBeNull();
  });

  it("says no agents bound rather than hiding a harness nobody uses", async () => {
    await show(
      fakeClient(async () => [harness({ id: "claude", default: true })], [member("ceo", "other")]),
    );
    expect(container.querySelector('[data-testid="local-harness-bound"]')!.textContent).toBe(
      "No agents bound",
    );
  });
});
