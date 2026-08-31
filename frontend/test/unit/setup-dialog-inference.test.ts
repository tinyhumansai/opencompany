// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { OpenCompanyClient } from "@/api/client";
import type { InferenceStatus } from "@/api/inference";
import type { TeamMemberDto } from "@/api/types";
import { SetupDialog } from "@/setup/SetupDialog";

/**
 * What first-run setup promises about the model, and what it does with the
 * answer (docs/spec/runtime/company-setup.md).
 *
 * Every case here is a way the disclosure can be *wrong*, which is worse than
 * absent: the whole point of saying something before the questions is that the
 * operator can trust it.
 *
 * - It must ask the **addressed company**, not the host. Readiness read off the
 *   host's managed credential warns a BYOK company about a model it is about to
 *   use, and refuses multi-company hosts outright.
 * - It must not be able to **hold the dialog shut**. Nothing dismisses this
 *   dialog — no Esc, no backdrop, no close button — so an unanswered readiness
 *   check is a locked console.
 * - "Set up a model" must not record a **skip**. It is the operator starting
 *   this flow, and suppressing the offer on their return strands them.
 * - The credential CTA must be gated on the host's **reason**. A model that
 *   answered and could not be designed from already had a working key.
 */

const HARNESS: InferenceStatus = {
  provider: "openrouter",
  slug: "openrouter",
  baseUrl: "https://example.invalid/v1",
  models: {},
  defaultTierModels: {},
  source: "manifest",
  keyConfigured: true,
  cognition: "harness",
  usageMetering: "perTurn",
  harnessReachable: true,
} as InferenceStatus;

const ECHO: InferenceStatus = { ...HARNESS, cognition: "echo" } as InferenceStatus;

/** One signed-in operator; the dialog asks the host which role theirs is. */
const ME_ADMIN = {
  id: "u1",
  email: "owner@example.invalid",
  role: "admin",
  company: "acme",
  hasPassword: false,
  mustChangePassword: false,
} as const;

/** One proposed agent is enough: the build-out's reveal is paced per agent. */
const AGENT = { name: "Ada", role: "Operations", description: "Runs the desk." };

function clientWith(
  over: {
    status?: () => Promise<InferenceStatus>;
    source?: "model" | "fallback";
    reason?: string | null;
    roster?: TeamMemberDto[];
    role?: "admin" | "member";
  } = {},
): OpenCompanyClient & { removed: string[] } {
  const removed: string[] = [];
  const role = over.role ?? "admin";
  return {
    scopeFor: (company: string | null) => `/api/v1/companies/${company}`,
    get: async (path: string) => {
      // The role read is a second `get` on a different route; a mock that
      // answers every path with the inference status would report the operator
      // as a non-admin and hide the very CTAs these tests exercise.
      if (path.endsWith("/auth/me")) return { ...ME_ADMIN, role };
      return over.status ? await over.status() : ECHO;
    },
    post: async () => ({
      agents: [AGENT],
      template: "ecommerce",
      source: over.source ?? "fallback",
      ...(over.reason === null ? {} : { reason: over.reason ?? "no_model" }),
    }),
    addTeamMember: async () => ({}),
    listTeam: async () => over.roster ?? [],
    removeTeamMember: async (agentId: string) => {
      removed.push(agentId);
    },
    removed,
  } as unknown as OpenCompanyClient & { removed: string[] };
}

let container: HTMLDivElement;
let root: Root;
let skipped: number;
let left: number;

beforeEach(() => {
  (globalThis as unknown as { IS_REACT_ACT_ENVIRONMENT: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
  skipped = 0;
  left = 0;
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
  vi.useRealTimers();
});

async function show(
  client: OpenCompanyClient,
  over: {
    redesign?: boolean;
    fallbackIds?: string[];
    onRedesign?: (fallbackIds: string[]) => void;
    onRetry?: (fallbackIds: string[]) => void;
    onReplacementComplete?: (fallbackIds: string[] | null) => void;
  } = {},
) {
  await act(async () => {
    root.render(
      createElement(SetupDialog, {
        open: true,
        client,
        company: "acme",
        redesign: over.redesign,
        fallbackIds: over.fallbackIds,
        onSkip: () => {
          skipped += 1;
        },
        onLeave: () => {
          left += 1;
        },
        onDone: () => {},
        onRedesign: over.onRedesign ?? (() => {}),
        onRetry: over.onRetry,
        onReplacementComplete: over.onReplacementComplete,
      }),
    );
  });
}

/** Base UI portals the dialog, so query the document rather than the container. */
const find = (testId: string) => document.querySelector(`[data-testid="${testId}"]`);

const linkNamed = (text: string) =>
  Array.from(document.querySelectorAll("a")).find((a) => a.textContent?.trim() === text);

async function click(el: Element) {
  await act(async () => {
    (el as HTMLElement).click();
  });
}

async function answer(testId: string, value: string) {
  const field = find(testId) as HTMLInputElement | HTMLTextAreaElement | null;
  expect(field, `no field ${testId}`).toBeTruthy();
  await act(async () => {
    const setter = Object.getOwnPropertyDescriptor(
      field instanceof HTMLTextAreaElement
        ? HTMLTextAreaElement.prototype
        : HTMLInputElement.prototype,
      "value",
    )!.set!;
    setter.call(field, value);
    field!.dispatchEvent(new Event("input", { bubbles: true }));
  });
  await click(find("setup-next")!);
}

/** Answer all three questions and let the build-out finish. */
async function runFlow() {
  await answer("setup-field-industry", "E-commerce — homeware");
  await answer("setup-field-teamHint", "");
  await answer("setup-field-automate", "");
  // The build-out reveals one agent per REVEAL_MS and lingers before finishing.
  for (let i = 0; i < 40 && !find("setup-finish"); i++) {
    await act(async () => {
      await new Promise((r) => setTimeout(r, 60));
    });
  }
  expect(find("setup-finish"), "build-out never finished").toBeTruthy();
}

describe("readiness is the addressed company's, not the host's", () => {
  it("says nothing when this company's own cognition can design a roster", async () => {
    await show(clientWith({ status: async () => HARNESS }));
    expect(find("setup-question")).toBeTruthy();
    // The BYOK case: this company has a model even where the host holds no
    // managed credential, so warning about a missing one would be a lie.
    expect(find("setup-inference-notice")).toBeNull();
  });

  it("warns when this company booted onto a path with no design pass", async () => {
    await show(clientWith({ status: async () => ECHO }));
    expect(find("setup-inference-notice")?.textContent).toContain("can't reach a model");
  });

  it("does not promise a tailored team when readiness could not be read", async () => {
    await show(clientWith({ status: async () => Promise.reject(new Error("nope")) }));
    expect(find("setup-inference-notice")?.textContent).toContain("couldn't check");
  });

  it("omits the model CTA when this host can never run the design pass", async () => {
    await show(
      clientWith({ status: async () => ({ ...ECHO, harnessReachable: false }) }),
    );
    // A credential cannot put a harness-less binary on the design path, so the
    // CTA that promises otherwise would be a dead end — say so instead. The
    // copy says the roster cannot be *designed* with a model, not that no model
    // can run: a `hosted`/`sidecar`/`custom` deployment runs one while still
    // lacking the local roster builder.
    expect(find("setup-inference-notice")?.textContent).toContain(
      "can't design your team with a model",
    );
    expect(linkNamed("Set up a model")).toBeUndefined();
  });

  it("does not claim no model can run when a hosted brain is the one without the design pass", async () => {
    // A `hosted`/`sidecar`/`custom` deployment runs a model — this binary just
    // cannot design a roster from it. "can't run a model" would be a lie, so the
    // copy says the team can't be designed with one instead.
    for (const cognition of ["hosted", "sidecar", "custom"] as const) {
      await show(
        clientWith({
          status: async () =>
            ({ ...ECHO, cognition, harnessReachable: false }) as InferenceStatus,
        }),
      );
      const notice = find("setup-inference-notice");
      expect(notice?.textContent).toContain("can't design your team with a model");
      expect(notice?.textContent).not.toContain("can't run a model");
    }
  });

  it("names the restart a configured company is waiting on instead of offering to set up a model again", async () => {
    // A company that booted with no inference source and had a credential saved
    // afterwards is *configured* — offering "Set up a model" again would send
    // the operator round a loop only a restart can close.
    await show(
      clientWith({
        status: async () => ({ ...ECHO, restartRequired: true, harnessReachable: true }),
      }),
    );
    const notice = find("setup-inference-notice");
    expect(notice?.textContent).toContain("needs a restart");
    expect(notice?.textContent).not.toContain("can't reach a model");
    const restartLink = linkNamed("Restart the company");
    expect(restartLink).toBeTruthy();
    expect(restartLink).toHaveProperty("hash", "#/settings/connections");

    // Following it is starting setup, not declining it — same as "Set up a
    // model", so the return reopens the dialog.
    await click(restartLink!);
    expect(left, "should close for the navigation").toBe(1);
    expect(skipped, 'must not persist "I\'ll do this later"').toBe(0);
  });

  it("does not offer the model CTA to a member who cannot land it", async () => {
    // The Connections inference form is an admin's; a member is told to ask
    // rather than handed a link that can only 403.
    await show(clientWith({ status: async () => ECHO, role: "member" }));
    expect(find("setup-inference-notice")?.textContent).toContain(
      "Ask an admin to set up a model",
    );
    expect(linkNamed("Set up a model")).toBeUndefined();
  });

  it("tells a member on a restart-pending company to ask an admin to restart", async () => {
    await show(
      clientWith({
        status: async () => ({ ...ECHO, restartRequired: true, harnessReachable: true }),
        role: "member",
      }),
    );
    expect(find("setup-inference-notice")?.textContent).toContain(
      "Ask an admin to restart the company",
    );
    expect(linkNamed("Restart the company")).toBeUndefined();
  });

  it("withholds the model CTA until the host names the operator's role", async () => {
    // canManage is tri-state: while the role read is outstanding — and if it
    // fails — the admin actions stay withheld rather than guessed. A member must
    // never be handed the Settings link off a default that read as admin.
    let resolveRole: (who: unknown) => void = () => {};
    const roleRead = new Promise<unknown>((resolve) => {
      resolveRole = resolve;
    });
    const client = {
      ...clientWith({}),
      get: (path: string) => {
        if (path.endsWith("/auth/me")) return roleRead;
        return Promise.resolve(ECHO);
      },
    } as unknown as OpenCompanyClient;

    await show(client);
    expect(find("setup-inference-notice")?.textContent).toContain("can't reach a model");
    expect(
      linkNamed("Set up a model"),
      "offered the admin action before the role read settled",
    ).toBeUndefined();

    await act(async () => {
      resolveRole(ME_ADMIN);
    });
    expect(linkNamed("Set up a model")).toBeTruthy();
  });

  it("ignores a delayed status from the company the dialog was on before", async () => {
    // The readiness check answers the *addressed* company. Switching companies
    // cleans up the old check, but a response already in flight still resolves —
    // without a guard it would overwrite the new company's reachability with the
    // old host's answer, hiding the Settings CTA (or showing it) wrongly.
    const statuses: Record<string, (s: InferenceStatus) => void> = {};
    const asked: string[] = [];
    const client = {
      ...clientWith({}),
      get: async (path: string) => {
        if (path.endsWith("/auth/me")) return ME_ADMIN;
        const company = path.split("/").at(-2) ?? "?";
        asked.push(company);
        return new Promise<InferenceStatus>((resolve) => {
          statuses[company] = resolve;
        });
      },
    } as unknown as OpenCompanyClient;

    const render = (company: string) =>
      act(async () => {
        root.render(
          createElement(SetupDialog, {
            open: true,
            client,
            company,
            onSkip: () => {},
            onLeave: () => {},
            onDone: () => {},
            onRedesign: () => {},
          }),
        );
      });

    await render("acme");
    await render("globex");
    expect(asked).toEqual(["acme", "globex"]);

    // Globex's host answers first: harness reachable, but on a path with no
    // design pass — the notice offers the credential CTA.
    await act(async () => {
      statuses["globex"]({ ...ECHO, harnessReachable: true });
    });
    expect(find("setup-inference-notice")?.textContent).toContain("can't reach a model");
    expect(linkNamed("Set up a model")).toBeTruthy();

    // The acme response — the check the effect was cleaned up over — lands
    // late. Its harness-less answer must not flip the notice to "can't run a
    // model" and hide the CTA.
    await act(async () => {
      statuses["acme"]({ ...ECHO, harnessReachable: false });
    });
    expect(find("setup-inference-notice")?.textContent).toContain("can't reach a model");
    expect(linkNamed("Set up a model")).toBeTruthy();
  });
});

describe("a stalled readiness check cannot lock the operator in", () => {
  it("gives up on the check and asks the questions anyway", async () => {
    vi.useFakeTimers();
    // A host that never answers: `fetch` has no timeout of its own, and this
    // dialog ignores Esc, the backdrop and the close button.
    await show(clientWith({ status: () => new Promise<InferenceStatus>(() => {}) }));
    expect(find("setup-inference-check"), "should be waiting at first").toBeTruthy();
    expect(find("setup-question")).toBeNull();

    await act(async () => {
      await vi.advanceTimersByTimeAsync(10_000);
    });

    expect(find("setup-inference-check")).toBeNull();
    expect(find("setup-question"), "questions withheld indefinitely").toBeTruthy();
    expect(find("setup-inference-notice")?.textContent).toContain("couldn't check");
  });
});

describe('"Set up a model" is starting setup, not declining it', () => {
  it("closes without recording the skip that would suppress the offer", async () => {
    await show(clientWith());
    const link = linkNamed("Set up a model");
    expect(link, "no model link on the notice").toBeTruthy();
    expect(link).toHaveProperty("hash", "#/settings/connections");

    await click(link!);

    expect(left, "should close for the navigation").toBe(1);
    expect(skipped, 'must not persist "I\'ll do this later"').toBe(0);
  });

  it('still records a skip for the explicit "I\'ll do this later"', async () => {
    await show(clientWith());
    await click(find("setup-skip")!);
    expect(skipped).toBe(1);
    expect(left).toBe(0);
  });
});

describe("the finished build-out points at what would actually help", () => {
  it("offers the credential CTA when no model was reachable", async () => {
    await show(clientWith({ source: "fallback", reason: "no_model" }));
    await runFlow();
    expect(find("setup-add-model")).toBeTruthy();
    expect(find("setup-buildout-title")?.parentElement?.textContent).toContain(
      "couldn't reach a model",
    );
  });

  it("asks for more detail instead when a model answered but was not designable", async () => {
    await show(clientWith({ source: "fallback", reason: "not_designable" }));
    await runFlow();
    // The key already worked. Sending this operator to Settings is an
    // instruction that cannot help them.
    expect(find("setup-add-model"), "sent to fix a credential that worked").toBeNull();
    // The fix is a better answer, and the retry makes it reachable in place —
    // the Company page's prompt is hidden once the fallback team staffs the
    // company, so "rerun from the Company page" alone would be an unreachable
    // instruction.
    expect(find("setup-try-redesign"), "no way to answer again").toBeTruthy();
    expect(find("setup-buildout-title")?.parentElement?.textContent).toContain(
      "enough in your answers",
    );
  });

  it('"Try again" on a not-designable fallback restarts the questions so the answers can be richer', async () => {
    await show(clientWith({ source: "fallback", reason: "not_designable" }));
    await runFlow();
    await click(find("setup-try-redesign")!);
    expect(find("setup-question")).toBeTruthy();
    // The fallback team was already created, so the next build-out replaces it.
    expect(find("setup-redesign-notice")).toBeTruthy();
  });

  it("withholds the credential CTA when the host did not say why", async () => {
    await show(clientWith({ source: "fallback", reason: null }));
    await runFlow();
    expect(find("setup-add-model")).toBeNull();
  });

  it("withholds the credential CTA on a host that cannot run a model", async () => {
    await show(
      clientWith({
        source: "fallback",
        reason: "no_model",
        status: async () => ({ ...ECHO, harnessReachable: false }),
      }),
    );
    await runFlow();
    // A credential cannot move a harness-less binary onto the design path, so
    // the CTA that promises it would send the operator round a redesign loop
    // that cannot end.
    expect(find("setup-add-model")).toBeNull();
  });

  it("tells a member to ask an admin instead of offering the wiring CTA", async () => {
    await show(clientWith({ source: "fallback", reason: "no_model", role: "member" }));
    await runFlow();
    // The Connections card is read-only for a member, so the link that sends
    // them there can only 403 — the admin-directed copy takes its place.
    expect(find("setup-add-model"), "handed a dead-end wiring CTA").toBeNull();
    expect(find("setup-add-model-member")?.textContent).toContain(
      "Ask an admin to add a model",
    );
  });

  it("says nothing about fallbacks when the model designed the team", async () => {
    await show(clientWith({ source: "model", reason: null }));
    await runFlow();
    expect(find("setup-add-model")).toBeNull();
    expect(find("setup-buildout-title")?.textContent).toContain("Your starting team is ready");
  });

  it("offers a retry and a connection check when a wired model was unreachable", async () => {
    await show(clientWith({ source: "fallback", reason: "model_unreachable" }));
    await runFlow();
    // A credential is already wired, so the CTA that fixes a *missing* model
    // cannot help — but the failure could be the credential itself (rejected
    // rather than merely busy), so the route that checks it is offered beside
    // the retry that fixes a transient blip.
    expect(find("setup-add-model")).toBeNull();
    expect(find("setup-try-redesign")).toBeTruthy();
    expect(find("setup-check-connection")).toBeTruthy();
    expect(find("setup-buildout-title")?.parentElement?.textContent).toContain(
      "couldn't reach it just now",
    );
  });

  it("withholds the connection check from a member who cannot change it", async () => {
    await show(clientWith({ source: "fallback", reason: "model_unreachable", role: "member" }));
    await runFlow();
    // The retry is still theirs — a transient blip may have passed. The
    // Settings link is not: a member cannot land the connection check it
    // advertises, so the admin-directed copy takes its place.
    expect(find("setup-check-connection"), "handed a dead-end settings link").toBeNull();
    expect(find("setup-try-redesign")).toBeTruthy();
    expect(find("setup-check-connection-member")?.textContent).toContain(
      "Ask an admin to check the connection",
    );
  });

  it('"Check connection in Settings" records the redesign debt like the wiring CTA', async () => {
    const redesigned: string[][] = [];
    await show(clientWith({ source: "fallback", reason: "model_unreachable" }), {
      onRedesign: (ids) => redesigned.push(ids),
    });
    await runFlow();
    await click(find("setup-check-connection")!);
    // The fallback team is to be replaced when the operator returns from
    // checking the connection — the same debt the wiring CTA records, because
    // the next build-out must replace the shipped team, not stack on it.
    expect(redesigned).toEqual([[]]);
  });

  it('"Try again" returns to the questions in replacing mode', async () => {
    let creations = 0;
    const client = clientWith({ source: "fallback", reason: "model_unreachable" });
    client.addTeamMember = async () => {
      creations += 1;
      return ({ id: `fallback-${creations}`, role: "Operations" }) as TeamMemberDto;
    };
    await show(client);
    await runFlow();
    expect(creations).toBe(1);
    await click(find("setup-try-redesign")!);
    expect(find("setup-question")).toBeTruthy();
    // The company now carries the standard team, so the next build-out must
    // replace it — the notice makes that consequence visible before answering.
    expect(find("setup-redesign-notice")).toBeTruthy();
    // The retry must be allowed through the build-out guard a second time:
    // without the reset in tryRedesign the dialog stalls on "Creating your
    // team…" with no effect entering to build anything.
    await runFlow();
    expect(find("setup-finish")).toBeTruthy();
    expect(creations).toBe(2);
  });

  it('"Try again" persists the redesign debt the retry is owed', async () => {
    const retried: string[][] = [];
    const client = clientWith({ source: "fallback", reason: "model_unreachable" });
    client.addTeamMember = async () =>
      ({ id: "fallback-1", role: "Operations" }) as TeamMemberDto;
    await show(client, { onRetry: (ids) => retried.push(ids) });
    await runFlow();
    await click(find("setup-try-redesign")!);
    // The fallback team is on the roster, so a reload before the replacement
    // completes must be able to reopen in redesign mode — otherwise the gate
    // reads staffed and the promised retry is unreachable.
    expect(retried).toEqual([["fallback-1"]]);
  });
});

describe("a replacing build-out clears the team it replaces", () => {
  it("removes operator-staffed teammates before creating the new roster", async () => {
    const client = clientWith({
      source: "fallback",
      reason: "model_unreachable",
      roster: [
        { id: "a1", name: "Old Ada", role: "Operations", global: false } as TeamMemberDto,
        { id: "b2", name: "Baseline", role: "Founder", global: true } as TeamMemberDto,
      ],
    });
    // `fallbackIds` is the fallback team the previous pass created, captured
    // when the operator left for model settings — in this direct render it is
    // named by hand rather than read from the debt.
    await show(client, { redesign: true, fallbackIds: ["a1"] });
    await runFlow();
    // The baseline survives — it is not on the operator's roster and cannot be
    // deleted anyway. Only the fallback team the last pass created goes.
    expect(client.removed).toEqual(["a1"]);
  });

  it("removes nothing on a first-run build-out", async () => {
    const client = clientWith({ source: "fallback", reason: "model_unreachable" });
    await show(client);
    await runFlow();
    expect(client.removed).toEqual([]);
  });

  it("a 'Try again' retry removes only the rows the first pass created", async () => {
    const removed: string[] = [];
    const roster = [
      { id: "f1", name: "Fallback", role: "Operations", global: false } as TeamMemberDto,
      { id: "h2", name: "Hand", role: "Designer", global: false } as TeamMemberDto,
    ];
    const client = {
      ...clientWith({ source: "fallback", reason: "model_unreachable" }),
      // The first pass's creates are answered with the ids they were assigned,
      // so the retry's replacement is bounded to exactly those rows.
      addTeamMember: async () => ({ id: "f1" }),
      listTeam: async () => roster,
      removeTeamMember: async (agentId: string) => {
        removed.push(agentId);
      },
      removed,
    } as unknown as OpenCompanyClient & { removed: string[] };
    await show(client);
    await runFlow(); // first pass creates the fallback team
    await click(find("setup-try-redesign")!);
    await runFlow(); // retry: replaces that team
    // The hand-added teammate h2 is not on the first pass's boundary and must
    // survive; without the fallback to createdIds the retry would delete it.
    expect(removed).toEqual(["f1"]);
  });

  it("a redesign 'Try again' replaces the team the last pass created, not a stale boundary", async () => {
    const removed: string[] = [];
    const roster: TeamMemberDto[] = [
      { id: "old-1", name: "Fallback", role: "Operations", global: false } as TeamMemberDto,
    ];
    let created = 0;
    const client = {
      ...clientWith({ source: "fallback", reason: "model_unreachable" }),
      addTeamMember: async () => {
        const id = created++ === 0 ? "new-1" : "new-2";
        roster.push({ id, role: "Operations", global: false } as TeamMemberDto);
        return { id };
      },
      listTeam: async () => [...roster],
      removeTeamMember: async (agentId: string) => {
        removed.push(agentId);
        const idx = roster.findIndex((m) => m.id === agentId);
        if (idx >= 0) roster.splice(idx, 1);
      },
      removed,
    } as unknown as OpenCompanyClient & { removed: string[] };
    // The dialog reopens in redesign mode with the *original* fallback debt;
    // that boundary is stale once the first redesign build-out has swept it.
    await show(client, { redesign: true, fallbackIds: ["old-1"] });
    await runFlow(); // first redesign: sweeps old-1, creates new-1, but model_unreachable
    expect(removed, "the original fallback should go first").toEqual(["old-1"]);
    await click(find("setup-try-redesign")!);
    await runFlow(); // retry: must sweep the first redesign's own creation
    // Without the reset in tryRedesign, the retry's boundary stays the stale
    // old-1 (already gone), so the sweep finds nothing and the first redesign's
    // fallback team is left stacked beside the second pass's — a duplicate.
    expect(removed, "the first redesign's fallback should be replaced").toEqual([
      "old-1",
      "new-1",
    ]);
  });

  it("a designed replacement settles the redesign debt before the completion screen", async () => {
    const settled: (string[] | null)[] = [];
    const client = clientWith({
      source: "model",
      roster: [
        { id: "f1", name: "Fallback", role: "Operations", global: false } as TeamMemberDto,
      ],
    });
    await show(client, {
      redesign: true,
      fallbackIds: ["f1"],
      onReplacementComplete: (ids) => settled.push(ids),
    });
    await runFlow();
    // The redesign replaced the fallback with a designed team: the debt is
    // paid. Without this, a reload on the completion screen reopens redesign
    // with the swept boundary and stacks a duplicate beside the replacement.
    expect(settled).toEqual([null]);
    expect(client.removed).toEqual(["f1"]);
  });

  it("a replacement that falls back again re-keys the debt to the new fallback's rows", async () => {
    const settled: (string[] | null)[] = [];
    const roster = [
      { id: "old-1", name: "Fallback", role: "Operations", global: false } as TeamMemberDto,
    ];
    let nextId = 0;
    const client = {
      ...clientWith({ source: "fallback", reason: "no_model" }),
      addTeamMember: async () => {
        const id = `n${++nextId}`;
        roster.push({ id, role: "Operations", global: false } as TeamMemberDto);
        return { id };
      },
      listTeam: async () => [...roster],
      removed: [] as string[],
      removeTeamMember: async (agentId: string) => {
        client.removed.push(agentId);
        const idx = roster.findIndex((m) => m.id === agentId);
        if (idx >= 0) roster.splice(idx, 1);
      },
    } as unknown as OpenCompanyClient & { removed: string[] };
    await show(client, {
      redesign: true,
      fallbackIds: ["old-1"],
      onReplacementComplete: (ids) => settled.push(ids),
    });
    await runFlow();
    // The replacement also fell back (still no reachable model): the debt must
    // now name the new fallback's rows, so a reload resumes against them —
    // never the swept old-1, which would leave the new fallback stacked beside
    // a second pass that swept nothing.
    expect(settled).toEqual([["n1"]]);
    expect(client.removed).toEqual(["old-1"]);
  });

  it("a refused rollback re-keys the durable debt to include the row it could not remove", async () => {
    const settled: (string[] | null)[] = [];
    const removed: string[] = [];
    const roster = [
      { id: "f1", name: "Fallback", role: "Operations", global: false } as TeamMemberDto,
    ];
    const client = {
      ...clientWith(),
      post: async () => ({
        agents: [
          { name: "Ada", role: "Operations", description: "Runs the desk." },
          { name: "Bo", role: "Analyst", description: "Covers the numbers." },
        ],
        template: "ecommerce",
        source: "model",
      }),
      // One of two replacements lands, then the rollback of that row is refused
      // — the row survives and must be named by the durable boundary.
      addTeamMember: async (body: { role?: string }) => {
        if (body.role === "Analyst") throw new Error("nope");
        return { id: "n1" };
      },
      listTeam: async () => roster,
      removeTeamMember: async (agentId: string) => {
        removed.push(agentId);
        if (agentId === "n1") throw new Error("nope");
      },
      removed,
    } as unknown as OpenCompanyClient & { removed: string[] };
    await show(client, {
      redesign: true,
      fallbackIds: ["f1"],
      onReplacementComplete: (ids) => settled.push(ids),
    });
    await answer("setup-field-industry", "E-commerce — homeware");
    await answer("setup-field-teamHint", "");
    await answer("setup-field-automate", "");
    for (let i = 0; i < 40 && !find("setup-failed"); i++) {
      await act(async () => {
        await new Promise((r) => setTimeout(r, 60));
      });
    }
    expect(find("setup-failed")).toBeTruthy();
    // The kept row n1 survives the failed rollback; the durable debt must now
    // name it alongside the original boundary, so a reload before the retry
    // still replaces it rather than leaving it stacked beside the new team.
    expect(settled).toEqual([["f1", "n1"]]);
  });

  it("keeps the existing team when a replacing build-out creates nothing", async () => {
    const removed: string[] = [];
    const roster = [
      { id: "f1", name: "Fallback", role: "Operations", global: false } as TeamMemberDto,
    ];
    const client = {
      ...clientWith({ source: "fallback", reason: "model_unreachable" }),
      addTeamMember: async () => {
        throw new Error("nope");
      },
      listTeam: async () => roster,
      removeTeamMember: async (agentId: string) => {
        removed.push(agentId);
      },
      removed,
    } as unknown as OpenCompanyClient & { removed: string[] };
    await show(client, { redesign: true, fallbackIds: ["f1"] });
    await answer("setup-field-industry", "E-commerce — homeware");
    await answer("setup-field-teamHint", "");
    await answer("setup-field-automate", "");
    for (let i = 0; i < 40 && !find("setup-failed"); i++) {
      await act(async () => {
        await new Promise((r) => setTimeout(r, 60));
      });
    }
    // Every add was refused, so no replacement ever existed — the fallback team
    // is kept and the flow fails rather than claiming a completed redesign.
    expect(find("setup-failed")).toBeTruthy();
    expect(removed).toEqual([]);
  });

  it("rolls back a partially-landed replacement and keeps the existing team", async () => {
    const removed: string[] = [];
    const roster = [
      { id: "f1", name: "Fallback", role: "Operations", global: false } as TeamMemberDto,
    ];
    const client = {
      ...clientWith({ source: "fallback", reason: "model_unreachable" }),
      post: async () => ({
        agents: [
          { name: "Ada", role: "Operations", description: "Runs the desk." },
          { name: "Bo", role: "Analyst", description: "Covers the numbers." },
        ],
        template: "ecommerce",
        source: "model",
      }),
      // The replacement has two agents; the first lands, the second is refused.
      addTeamMember: async (body: { role?: string }) => {
        if (body.role === "Analyst") throw new Error("nope");
        return { id: "n1" };
      },
      listTeam: async () => roster,
      removeTeamMember: async (agentId: string) => {
        removed.push(agentId);
      },
      removed,
    } as unknown as OpenCompanyClient & { removed: string[] };
    await show(client, { redesign: true, fallbackIds: ["f1"] });
    await answer("setup-field-industry", "E-commerce — homeware");
    await answer("setup-field-teamHint", "");
    await answer("setup-field-automate", "");
    for (let i = 0; i < 40 && !find("setup-failed"); i++) {
      await act(async () => {
        await new Promise((r) => setTimeout(r, 60));
      });
    }
    // One of two replacements landed, so the redesign is not complete — trading
    // a complete fallback team for a single new teammate would leave the company
    // worse off. The flow fails and the partial row is rolled back, so the
    // redesign is atomic: the company is exactly as it was before the attempt.
    expect(find("setup-failed")).toBeTruthy();
    expect(removed).toEqual(["n1"]);
  });

  it("rolls back the new team and keeps the debt when the sweep of the old one fails", async () => {
    const removed: string[] = [];
    const settled: (string[] | null)[] = [];
    const roster = [
      { id: "f1", name: "Fallback", role: "Operations", global: false } as TeamMemberDto,
    ];
    const client = {
      ...clientWith({ source: "model" }),
      post: async () => ({
        agents: [
          { name: "Ada", role: "Operations", description: "Runs the desk." },
          { name: "Bo", role: "Analyst", description: "Covers the numbers." },
        ],
        template: "ecommerce",
        source: "model",
      }),
      addTeamMember: async (body: { role?: string }) => {
        const id = body.role === "Analyst" ? "n2" : "n1";
        roster.push({ id, role: body.role ?? "", global: false } as TeamMemberDto);
        return { id };
      },
      listTeam: async () => roster,
      removeTeamMember: async (agentId: string) => {
        removed.push(agentId);
        // The sweep of the old team fails: the new team is in place, but the old
        // row survives — the redesign must not report completion over it.
        if (agentId === "f1") throw new Error("nope");
      },
      removed,
    } as unknown as OpenCompanyClient & { removed: string[] };
    await show(client, {
      redesign: true,
      fallbackIds: ["f1"],
      onReplacementComplete: (ids) => settled.push(ids),
    });
    await answer("setup-field-industry", "E-commerce — homeware");
    await answer("setup-field-teamHint", "");
    await answer("setup-field-automate", "");
    for (let i = 0; i < 40 && !find("setup-failed"); i++) {
      await act(async () => {
        await new Promise((r) => setTimeout(r, 60));
      });
    }
    // The new rows are rolled back so the company is left with the old team, and
    // the flow fails rather than claiming a completed replacement.
    expect(find("setup-failed")).toBeTruthy();
    expect(removed).toEqual(["f1", "n1", "n2"]);
    // The debt is kept: onReplacementComplete was never asked to settle it, so a
    // reload before the retry still reopens the redesign over the old boundary.
    expect(settled).toEqual([]);
  });

  it("a retry after a refused rollback still replaces every row it must", async () => {
    const removed: string[] = [];
    // The roster the host reports grows as adds land; the second run's sweep
    // iterates it, so it must reflect the row the first run left behind.
    const roster = [
      { id: "f1", name: "Fallback", role: "Operations", global: false } as TeamMemberDto,
    ];
    let firstRun = true;
    let nextId = 0;
    const ids = ["n1", "n2", "n3"];
    const client = {
      ...clientWith({ source: "fallback", reason: "model_unreachable" }),
      post: async () => ({
        agents: [
          { name: "Ada", role: "Operations", description: "Runs the desk." },
          { name: "Bo", role: "Analyst", description: "Covers the numbers." },
        ],
        template: "ecommerce",
        source: "model",
      }),
      // The first replacement fails on its second write; the retry's writes all
      // land, so the retry reaches the sweep the failure never could.
      addTeamMember: async (body: { name?: string; role?: string }) => {
        if (firstRun && body.role === "Analyst") throw new Error("nope");
        const id = ids[nextId++];
        roster.push({ id, name: body.name ?? "", role: body.role ?? "", global: false } as TeamMemberDto);
        return { id };
      },
      listTeam: async () => roster,
      removeTeamMember: async (agentId: string) => {
        removed.push(agentId);
        // The first run's rollback of its partial row is refused — the row stays
        // live, so the retry must bound over it. The retry's sweep removes land.
        if (firstRun && agentId === "n1") throw new Error("nope");
      },
      removed,
    } as unknown as OpenCompanyClient & { removed: string[] };
    await show(client, { redesign: true, fallbackIds: ["f1"] });
    await answer("setup-field-industry", "E-commerce — homeware");
    await answer("setup-field-teamHint", "");
    await answer("setup-field-automate", "");
    for (let i = 0; i < 40 && !find("setup-failed"); i++) {
      await act(async () => {
        await new Promise((r) => setTimeout(r, 60));
      });
    }
    // The partial row was rolled back, and that delete was refused — the attempt
    // is still recorded, but n1 survives into the retry.
    expect(find("setup-failed")).toBeTruthy();
    expect(removed).toEqual(["n1"]);
    firstRun = false;

    await click(
      Array.from(document.querySelectorAll("button")).find(
        (b) => b.textContent?.trim() === "Try again",
      )!,
    );
    await answer("setup-field-industry", "E-commerce — homeware");
    await answer("setup-field-teamHint", "");
    await answer("setup-field-automate", "");
    for (let i = 0; i < 40 && !find("setup-finish"); i++) {
      await act(async () => {
        await new Promise((r) => setTimeout(r, 60));
      });
    }
    // The retry's sweep removes the fallback row the debt named *and* the row
    // the failed rollback could not remove — without the `kept` bookkeeping the
    // retry would bound only on the captured Set and strand n1.
    expect(find("setup-finish")).toBeTruthy();
    expect(removed).toEqual(["n1", "f1", "n1"]);
  });
});
