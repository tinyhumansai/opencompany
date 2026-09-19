// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { OpenCompanyClient } from "@/api/client";
import type { TeamMemberDto } from "@/api/types";
import { HarnessDetailDialog } from "@/components/harness-detail";
import { agentHref } from "@/lib/agent-profile";
import type { HarnessRow } from "@/lib/harnesses";

/**
 * The per-harness detail dialog — the surface that replaced the External
 * harnesses card, opened from both the agent editor and the Providers page.
 *
 * What is pinned here is the part that is a claim rather than a layout: which
 * teammates it says are on a harness, and the word it uses for them. The
 * readiness half is the caller's `useHarnessRows`, already covered where it
 * lives.
 */

let container: HTMLDivElement;
let root: Root;

function row(over: Partial<HarnessRow> = {}): HarnessRow {
  return { id: "laptop", label: "Claude Code", kind: "acp", isDefault: false, declared: true, ...over };
}

function member(id: string, harness?: string): TeamMemberDto {
  const base = { id, role: "Writer", name: id } as TeamMemberDto;
  return harness === undefined ? base : { ...base, harness };
}

function fakeClient(roster: TeamMemberDto[]): OpenCompanyClient {
  return { listTeam: async () => roster } as unknown as OpenCompanyClient;
}

async function open(props: {
  client: OpenCompanyClient;
  row: HarnessRow | null;
  defaultHarnessId?: string;
}) {
  await act(async () => {
    root.render(
      createElement(HarnessDetailDialog, {
        client: props.client,
        company: "acme",
        row: props.row,
        defaultHarnessId: props.defaultHarnessId,
        onRecheck: () => {},
        checking: false,
        install: async () => null,
        installing: new Set<string>(),
        installErrors: {},
        onOpenChange: () => {},
      }),
    );
  });
  await act(async () => {});
}

/** The dialog is portal-rendered, so it lands on the body rather than in `container`. */
function dialog(): HTMLElement {
  const found = document.body.querySelector('[data-testid="harness-detail"]');
  if (!found) throw new Error(`no dialog rendered; body was: ${document.body.innerHTML}`);
  return found as HTMLElement;
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

describe("harness detail dialog", () => {
  it("renders nothing until a harness is opened", async () => {
    await open({ client: fakeClient([]), row: null });
    expect(document.body.querySelector('[data-testid="harness-detail"]')).toBeNull();
  });

  it("lists the teammates bound to this harness, and counts them as bound", async () => {
    await open({
      client: fakeClient([member("ceo"), member("writer", "laptop"), member("jamie", "laptop")]),
      row: row({ id: "laptop" }),
      defaultHarnessId: "main",
    });
    const bound = dialog().querySelector('[data-testid="harness-detail-bound"]')!;
    expect(bound.textContent).toContain("2 agents bound");
    expect(bound.textContent).toContain("writer");
    expect(bound.textContent).toContain("jamie");
    // The teammate that declares nothing is on the default harness, not this one.
    expect(bound.textContent).not.toContain("ceo");
  });

  it("counts an undeclared teammate toward the default harness and says it inherits", async () => {
    await open({
      client: fakeClient([member("ceo"), member("writer", "laptop")]),
      row: row({ id: "main", label: "In-house", kind: "built_in", isDefault: true }),
      defaultHarnessId: "main",
    });
    const bound = dialog().querySelector('[data-testid="harness-detail-bound"]')!;
    expect(bound.textContent).toContain("1 agent bound");
    expect(bound.textContent).toContain("ceo");
    expect(bound.textContent).toContain("Inherits the default");
  });

  it("never calls a harness connected", async () => {
    // "Connected" is the Providers page's word for a credential this company
    // holds. A harness holds nothing company-wide, and borrowing the word here
    // would promise a shared persisted state that does not exist.
    await open({
      client: fakeClient([member("writer", "laptop")]),
      row: row({ id: "laptop" }),
      defaultHarnessId: "main",
    });
    expect(dialog().textContent).not.toMatch(/connected/i);
  });

  it("links each teammate to its own Model tab rather than editing here", async () => {
    await open({
      client: fakeClient([member("writer", "laptop")]),
      row: row({ id: "laptop" }),
      defaultHarnessId: "main",
    });
    // Through `agentHref`, so this is the canonical spelling of a teammate's
    // page rather than a second one the router merely still accepts.
    const link = dialog().querySelector('[data-testid="harness-detail-bound"] a');
    expect(link?.getAttribute("href")).toBe(agentHref("writer", { tab: "model" }));
    expect(link?.getAttribute("href")).toContain("tab=model");
  });

  it("says what a browser cannot see rather than reporting the CLI missing", async () => {
    // `readiness: undefined` is "nothing probed this machine". Rendering it as
    // "Not installed" would send someone to reinstall a CLI they already have.
    await open({ client: fakeClient([]), row: row(), defaultHarnessId: "main" });
    const status = dialog().querySelector('[data-testid="harness-detail-status"]')!;
    expect(status.textContent).toContain("Desktop only");
    expect(status.textContent).not.toContain("Not installed");
  });
});
