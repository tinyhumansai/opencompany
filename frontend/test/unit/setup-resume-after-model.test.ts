// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it } from "vitest";

import type { OpenCompanyClient } from "@/api/client";
import type { TeamMemberDto } from "@/api/types";
import { ConnectionScopeProvider } from "@/connections/ConnectionContext";
import type { ConnectionId, LocalScope } from "@/connections/types";
import { SetupController } from "@/setup/SetupController";
import { markSetupRedesign, markSetupSkipped, setupRedesign, setupResuming } from "@/setup/state";

/**
 * The way back into setup after leaving it to wire a model.
 *
 * Not recording a skip for that navigation is only half the fix. This
 * controller stays mounted across hash changes, its gate re-evaluates only on
 * `(client, company, scope, deepLinked)`, and `evaluatedOnce` bars a second
 * unprompted open — so with nothing else done, an operator who followed "Set up
 * a model", configured one, and came back would find no dialog and an unstaffed
 * company, reachable only through the Team page's separate prompt. That is the
 * same dead end the skip caused, arrived at differently.
 *
 * So the departure records a debt and the return pays it, on both the routes a
 * return can take: an ordinary hash change, and a full reload (wiring a provider
 * can ask for a restart).
 */

/** One connection's view of a single-company host. */
const SCOPE: LocalScope = { connection: "test-connection" as ConnectionId, company: null };

/** The baseline every company carries — present, and not "staffed". */
const BASELINE: TeamMemberDto[] = ["operations", "page_builder", "researcher", "writer"].map(
  (id) => ({ id, role: "Analyst", inboxEnabled: false, global: true }) as TeamMemberDto,
);

const STAFFED: TeamMemberDto[] = [
  ...BASELINE,
  { id: "ada", role: "Operations", inboxEnabled: false } as TeamMemberDto,
];

function clientWith(roster: TeamMemberDto[]): OpenCompanyClient {
  return {
    scopeFor: (company: string | null) => `/api/v1/companies/${company}`,
    listTeam: async () => roster,
    // The dialog's own readiness check; `echo` keeps the notice on screen. The
    // role read is a second `get` on `/auth/me`; answer it as an admin so the
    // model CTAs this spec follows are offered.
    get: async (path: string) =>
      path.endsWith("/auth/me") ? { role: "admin" } : { cognition: "echo" },
    post: async () => ({ agents: [], template: "ecommerce", source: "fallback" }),
  } as unknown as OpenCompanyClient;
}

let container: HTMLDivElement;
let root: Root;

beforeEach(() => {
  (globalThis as unknown as { IS_REACT_ACT_ENVIRONMENT: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
  localStorage.clear();
  window.location.hash = "#/overview";
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
  localStorage.clear();
});

async function mount(client: OpenCompanyClient, deepLinked = false) {
  await act(async () => {
    root.render(
      createElement(ConnectionScopeProvider, {
        scope: SCOPE,
        children: createElement(SetupController, { client, company: null, deepLinked }),
      }),
    );
  });
}

const dialog = () => document.querySelector('[data-testid="setup-dialog"]');
const find = (testId: string) => document.querySelector(`[data-testid="${testId}"]`);

const modelLink = () =>
  Array.from(document.querySelectorAll("a")).find((a) => a.textContent?.trim() === "Set up a model");

const addModelLink = () =>
  Array.from(document.querySelectorAll("a")).find(
    (a) => a.textContent?.trim() === "Add a model in Settings",
  );

/** Navigate as the console does — a hash change the controller can hear. */
async function goTo(hash: string) {
  await act(async () => {
    window.location.hash = hash;
    window.dispatchEvent(new HashChangeEvent("hashchange"));
  });
}

/** Answer the three questions and let the build-out finish. */
async function runFlow() {
  const setField = async (testId: string, value: string) => {
    const field = document.querySelector(`[data-testid="${testId}"]`) as
      | HTMLInputElement
      | HTMLTextAreaElement
      | null;
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
    await act(async () => {
      (document.querySelector('[data-testid="setup-next"]') as HTMLElement).click();
    });
  };
  await setField("setup-field-industry", "E-commerce — homeware");
  await setField("setup-field-teamHint", "");
  await setField("setup-field-automate", "");
  for (let i = 0; i < 40 && !addModelLink(); i++) {
    await act(async () => {
      await new Promise((r) => setTimeout(r, 60));
    });
  }
  expect(addModelLink(), "completion CTA never appeared").toBeTruthy();
}

/** Follow "Set up a model", which both closes the dialog and navigates. */
async function leaveForModelSettings() {
  const link = modelLink();
  expect(link, "no model link on the notice").toBeTruthy();
  await act(async () => {
    (link as HTMLElement).click();
  });
  // The anchor's onClick does not preventDefault, so jsdom queues its own
  // navigation to Settings as a deferred task. Drain it *before* driving the
  // hash change below — a macrotask queued before this flush must have run by
  // the time it lands, so it cannot later fire into a remount and revert the
  // address to Settings when a return (or reload) has moved it to the console.
  await act(async () => {
    await new Promise((r) => setTimeout(r, 0));
  });
  // jsdom does not follow the anchor's href, so drive the navigation it implies.
  await goTo("#/settings/connections");
}

describe("leaving to wire a model", () => {
  it("records a debt rather than a skip", async () => {
    await mount(clientWith(BASELINE));
    expect(dialog(), "setup should have opened by itself").toBeTruthy();

    await leaveForModelSettings();

    expect(dialog(), "should have closed for the navigation").toBeNull();
    expect(setupResuming(SCOPE)).toBe(true);
  });

  it("reopens setup when the operator navigates back", async () => {
    await mount(clientWith(BASELINE));
    await leaveForModelSettings();
    expect(dialog()).toBeNull();

    await goTo("#/overview");

    expect(dialog(), "the flow they went to enable is unreachable").toBeTruthy();
    expect(find("setup-question")).toBeTruthy();
  });

  it("stays shut while they are still on the settings page", async () => {
    await mount(clientWith(BASELINE));
    await leaveForModelSettings();
    // A sub-page of the same settings area is not "coming back".
    await goTo("#/settings/connections?provider=openrouter");
    expect(dialog()).toBeNull();
  });

  it("reopens after a reload on the way back, not one on the page itself", async () => {
    // Wiring a provider can ask for a restart, so the return is not always a
    // hash change. Two fresh mounts stand in for the two reloads.
    await mount(clientWith(BASELINE));
    await leaveForModelSettings();
    await act(async () => root.unmount());

    root = createRoot(container);
    // `deepLinked` as `AppShell` computes it for this address: a reload on
    // `#/settings/connections` is a named view, so nothing opens unprompted and
    // the resume is the only thing that could.
    await mount(clientWith(BASELINE), true);
    expect(dialog(), "reloaded on the settings page — they have not returned yet").toBeNull();

    window.location.hash = "#/overview";
    await act(async () => root.unmount());
    root = createRoot(container);
    await mount(clientWith(BASELINE), true);

    expect(dialog(), "a reload back on the console should resume setup").toBeTruthy();
  });

  it("does not reopen over a company someone else staffed meanwhile", async () => {
    await mount(clientWith(BASELINE));
    await leaveForModelSettings();
    await act(async () => root.unmount());

    root = createRoot(container);
    window.location.hash = "#/overview";
    await mount(clientWith(STAFFED), true);

    expect(dialog(), "a setup dialog over a team that already exists").toBeNull();
    expect(setupResuming(SCOPE), "debt should be dropped").toBe(
      false,
    );
  });

  it("re-reads the roster on a hash-change return instead of trusting the old answer", async () => {
    // A colleague staffs the company while the operator is wiring a model. The
    // hash-change return goes through the same `arrive` listener as any other,
    // which must re-read the roster — the empty answer captured before the
    // navigation is a snapshot, not a fact, and opening setup over a team that
    // now exists would stack a second one.
    const roster: TeamMemberDto[] = [...BASELINE];
    const client = clientWith(roster);
    await mount(client);
    await leaveForModelSettings();

    roster.push({ id: "ada", role: "Operations", inboxEnabled: false } as TeamMemberDto);

    await goTo("#/overview");

    expect(dialog(), "a setup dialog over a team that exists").toBeNull();
    expect(setupResuming(SCOPE), "debt should be dropped").toBe(false);
  });

  it("keeps the debt when the return's roster read fails, so a later return can retry", async () => {
    // A transient failure must not consume the resume. The dialog stays shut
    // because the roster is unknown, but the debt survives so the next arrival
    // or reload can retry — otherwise the flow the operator went to enable is
    // reachable again only through the Company-page prompt.
    let failing = false;
    const roster: TeamMemberDto[] = [...BASELINE];
    const client = {
      ...clientWith(roster),
      listTeam: async () => {
        if (failing) throw new Error("transient");
        return roster;
      },
    } as unknown as OpenCompanyClient;
    await mount(client);
    await leaveForModelSettings();

    failing = true;
    await goTo("#/overview");
    expect(dialog(), "unknown roster — must stay shut").toBeNull();
    expect(setupResuming(SCOPE), "debt must survive the failed read").toBe(true);

    // The next return reads successfully: the debt pays out and setup reopens.
    failing = false;
    await goTo("#/settings/connections");
    await goTo("#/overview");
    expect(dialog(), "the retried return should resume setup").toBeTruthy();
  });

  it("ignores a stale return read that resolves after the company switches", async () => {
    // A return from model settings starts a roster read. If the operator
    // switches companies before it resolves, the callback must not reopen setup
    // over the new company: the listener is removed on the switch, but the
    // in-flight read is not cancelled, so without a guard the callback's
    // `setOpen` would land on a controller rendering a company its read never
    // saw — and the dialog it opened would then run replacement against the
    // wrong company's roster.
    let acmeReads = 0;
    let resolveAcme!: (roster: TeamMemberDto[]) => void;
    const acmeRead = new Promise<TeamMemberDto[]>((resolve) => {
      resolveAcme = resolve;
    });
    const client = {
      ...clientWith([...BASELINE]),
      listTeam: async (company: string | null) => {
        // The second acme read — the one `arrive` starts on the return — is the
        // read we hold open. Every other read is served.
        if (company !== "acme") return [...STAFFED];
        acmeReads++;
        if (acmeReads >= 2) return acmeRead;
        return [...BASELINE];
      },
    } as unknown as OpenCompanyClient;
    const render = (company: string | null) =>
      act(async () => {
        root.render(
          createElement(ConnectionScopeProvider, {
            scope: SCOPE,
            children: createElement(SetupController, { client, company, deepLinked: false }),
          }),
        );
      });

    // Mount on acme; the gate read is served and setup offers itself.
    await render("acme");
    expect(dialog(), "setup should have opened").toBeTruthy();
    await leaveForModelSettings();

    // Return to acme: `arrive` starts the roster read we hold open.
    await goTo("#/overview");
    expect(dialog(), "return read in flight").toBeNull();

    // Switch to a second company before the read lands. The stale callback
    // must not open setup over it — the second company's own gate read is the
    // only thing allowed to decide that.
    await render("globex");
    await act(async () => {
      resolveAcme([...BASELINE]);
    });

    expect(dialog(), "stale return read reopened setup over the new company").toBeNull();
  });

  it('drops the debt when the operator then says "I\'ll do this later"', async () => {
    await mount(clientWith(BASELINE));
    await leaveForModelSettings();
    await goTo("#/overview");
    expect(dialog()).toBeTruthy();

    await act(async () => {
      (find("setup-skip") as HTMLElement).click();
    });

    expect(dialog()).toBeNull();
    expect(setupResuming(SCOPE)).toBe(false);

    // And it stays shut across a return trip: "later" means later.
    await goTo("#/settings/connections");
    await goTo("#/overview");
    expect(dialog()).toBeNull();
  });
});

describe("the skip still suppresses the unprompted offer", () => {
  it("does not open on a company the operator already skipped", async () => {
    markSetupSkipped(SCOPE);
    await mount(clientWith(BASELINE));
    expect(dialog()).toBeNull();
  });
});

describe("leaving the completion screen to wire a model", () => {
  it("records a redesign debt, and the return reopens in redesign mode", async () => {
    const client = {
      scopeFor: () => "/api/v1/companies/acme",
      listTeam: async () => [...BASELINE],
      get: async (path: string) =>
        path.endsWith("/auth/me") ? { role: "admin" } : { cognition: "echo" },
      post: async () => ({
        agents: [{ name: "Ada", role: "Operations", description: "Runs the desk." }],
        template: "ecommerce",
        source: "fallback",
        reason: "no_model",
      }),
      addTeamMember: async () => ({}),
    } as unknown as OpenCompanyClient;
    await mount(client);
    await runFlow();

    await act(async () => {
      (addModelLink() as HTMLElement).click();
    });

    // The completion CTA is a *navigation*, not a finish: the shipped team is
    // to be redesigned on the return, and the run must not be treated as done.
    expect(dialog(), "should close for the navigation").toBeNull();
    expect(setupRedesign(SCOPE), "no redesign debt recorded").toBe(true);

    await goTo("#/overview");

    expect(dialog(), "the redesign they were owed never reopened").toBeTruthy();
    expect(find("setup-redesign-notice"), "not reopened in replacing mode").toBeTruthy();
  });

  it("replaces only the fallback team when another operator staffs someone meanwhile", async () => {
    // The redesign's replacement is bounded by the team the first pass actually
    // created, captured when the operator left for model settings — not by the
    // roster as it reads on return. A colleague who staffs a teammate while
    // those settings were open is doing their own work; deleting that row would
    // remove someone who was never told they were provisional.
    const removed: string[] = [];
    const roster: TeamMemberDto[] = [...BASELINE];
    const client = {
      scopeFor: () => "/api/v1/companies/acme",
      listTeam: async () => [...roster],
      get: async (path: string) =>
        path.endsWith("/auth/me") ? { role: "admin" } : { cognition: "echo" },
      post: async () => ({
        agents: [
          { name: "Ada", role: "Operations", description: "Runs the desk." },
          { name: "Cara", role: "Support", description: "Answers the inbox." },
        ],
        template: "ecommerce",
        source: "fallback",
        reason: "no_model",
      }),
      addTeamMember: async (input: { name: string; role: string }) => {
        // The host mints ids from the name, unique against the roster it already
        // holds (issue #686) — a replacing build-out's creates run while the
        // rows they replace still exist, so a second Ada becomes "ada-2". The
        // replacement sweep below must not then remove the new rows.
        const base = input.name.toLowerCase();
        let id = base;
        for (let suffix = 2; roster.some((m) => m.id === id); suffix++) {
          id = `${base}-${suffix}`;
        }
        const member = { id, role: input.role, inboxEnabled: false } as TeamMemberDto;
        roster.push(member);
        return member;
      },
      removeTeamMember: async (id: string) => {
        removed.push(id);
      },
    } as unknown as OpenCompanyClient;
    await mount(client);
    await runFlow();

    // Leave for model settings, persisting the fallback team as the redesign's
    // boundary — the first pass created exactly two teammates.
    await act(async () => {
      (addModelLink() as HTMLElement).click();
    });
    expect(setupRedesign(SCOPE)).toBe(true);

    // A colleague staffs a teammate while the settings page is open.
    roster.push({ id: "bob", role: "Support", inboxEnabled: false } as TeamMemberDto);

    // Return: the redesign reopens, and the second build-out must replace only
    // the fallback team — ada and cara — leaving bob, someone else's work, alone.
    await goTo("#/overview");
    expect(find("setup-redesign-notice"), "not reopened in replacing mode").toBeTruthy();
    await runFlow();

    expect(removed, "the fallback team should be replaced").toEqual(["ada", "cara"]);
    expect(removed, "a teammate staffed while settings were open must survive").not.toContain("bob");
  });

  it("does not reopen a redesign whose fallback team another operator already replaced", async () => {
    // The redesign debt names the team the first pass created. If a colleague
    // replaces that team while model settings are open, the debt is stale —
    // reopening redesign would build a full roster and sweep nothing (the
    // recorded rows are gone), stacking a second team over their work.
    const roster: TeamMemberDto[] = [...BASELINE];
    const client = {
      scopeFor: () => "/api/v1/companies/acme",
      listTeam: async () => [...roster],
      get: async (path: string) =>
        path.endsWith("/auth/me") ? { role: "admin" } : { cognition: "echo" },
      post: async () => ({
        agents: [{ name: "Ada", role: "Operations", description: "Runs the desk." }],
        template: "ecommerce",
        source: "fallback",
        reason: "no_model",
      }),
      addTeamMember: async () => ({ id: "ada" }),
    } as unknown as OpenCompanyClient;
    await mount(client);
    await runFlow();

    await act(async () => {
      (addModelLink() as HTMLElement).click();
    });
    expect(setupRedesign(SCOPE)).toBe(true);

    // The colleague replaces the fallback team with their own while the
    // settings page is open: the row the debt names is gone.
    roster.push({ id: "zoe", role: "Operations", inboxEnabled: false } as TeamMemberDto);

    await goTo("#/overview");

    expect(dialog(), "setup over a team the redesign no longer names").toBeNull();
    expect(setupRedesign(SCOPE), "the stale debt should be dropped").toBe(false);
  });

  it("reopens as a first run, not a redesign, when the fallback team was deleted", async () => {
    // The debt's team is gone and nobody has staffed the company since — the
    // return should offer first-run setup, not a redesign over nothing. A
    // redesign here would claim a replacement while sweeping zero rows.
    const roster: TeamMemberDto[] = [...BASELINE];
    const client = {
      scopeFor: () => "/api/v1/companies/acme",
      listTeam: async () => [...roster],
      get: async (path: string) =>
        path.endsWith("/auth/me") ? { role: "admin" } : { cognition: "echo" },
      post: async () => ({
        agents: [{ name: "Ada", role: "Operations", description: "Runs the desk." }],
        template: "ecommerce",
        source: "fallback",
        reason: "no_model",
      }),
      addTeamMember: async () => ({ id: "ada" }),
    } as unknown as OpenCompanyClient;
    await mount(client);
    await runFlow();

    await act(async () => {
      (addModelLink() as HTMLElement).click();
    });
    expect(setupRedesign(SCOPE)).toBe(true);

    // The fallback team is gone entirely — the company is empty again.
    await goTo("#/overview");

    expect(dialog(), "the return should offer setup").toBeTruthy();
    expect(find("setup-redesign-notice"), "reopened as a redesign over nothing").toBeNull();
    expect(setupRedesign(SCOPE), "the stale debt should be dropped").toBe(false);
  });

  it("keeps the redesign debt across a reload after the return reopens it", async () => {
    // The redesign reopens the moment the operator returns, but the replacement
    // build-out has not run yet — the fallback team is still what the gate
    // calls staffed. A reload or crash in that window must not lose the debt:
    // without it the ordinary gate offers nothing and the owed redesign is
    // unreachable.
    const client = {
      scopeFor: () => "/api/v1/companies/acme",
      listTeam: async () => [...BASELINE],
      get: async (path: string) =>
        path.endsWith("/auth/me") ? { role: "admin" } : { cognition: "echo" },
      post: async () => ({
        agents: [{ name: "Ada", role: "Operations", description: "Runs the desk." }],
        template: "ecommerce",
        source: "fallback",
        reason: "no_model",
      }),
      addTeamMember: async () => ({}),
    } as unknown as OpenCompanyClient;
    await mount(client);
    await runFlow();

    await act(async () => {
      (addModelLink() as HTMLElement).click();
    });
    // The anchor's default is a navigation to Settings; jsdom follows it as a
    // deferred task, so absorb that before driving the return — otherwise it
    // can land mid-reload and leave the address on Settings when the fresh
    // mount re-evaluates the gate.
    await act(async () => {
      await new Promise((r) => setTimeout(r, 0));
    });
    expect(setupRedesign(SCOPE)).toBe(true);

    await goTo("#/overview");
    expect(find("setup-redesign-notice"), "not reopened in replacing mode").toBeTruthy();
    // The debt outlived the reopen.
    expect(setupRedesign(SCOPE), "the return paid the redesign debt").toBe(true);

    // A fresh mount stands in for the reload mid-redesign. The kept debt is the
    // only thing that can reopen replacing mode: `deepLinked` suppresses the
    // ordinary gate, and the fallback roster reads as staffed.
    await act(async () => root.unmount());
    root = createRoot(container);
    await mount(client, true);

    expect(dialog(), "the owed redesign did not come back after a reload").toBeTruthy();
    expect(find("setup-redesign-notice"), "not reopened in replacing mode").toBeTruthy();
    expect(setupRedesign(SCOPE)).toBe(true);
  });

  it("drops the redesign debt when the operator says 'I'll do this later' mid-redesign", async () => {
    // Skip is an explicit decline, not a hold: the debt must not re-offer a
    // redesign the operator just turned down.
    const client = {
      scopeFor: () => "/api/v1/companies/acme",
      listTeam: async () => [...BASELINE],
      get: async (path: string) =>
        path.endsWith("/auth/me") ? { role: "admin" } : { cognition: "echo" },
      post: async () => ({
        agents: [{ name: "Ada", role: "Operations", description: "Runs the desk." }],
        template: "ecommerce",
        source: "fallback",
        reason: "no_model",
      }),
      addTeamMember: async () => ({}),
    } as unknown as OpenCompanyClient;
    await mount(client);
    await runFlow();

    await act(async () => {
      (addModelLink() as HTMLElement).click();
    });
    // Absorb the anchor's deferred Settings navigation so it cannot fire into
    // the skip click below.
    await act(async () => {
      await new Promise((r) => setTimeout(r, 0));
    });
    expect(setupRedesign(SCOPE)).toBe(true);

    await goTo("#/overview");
    expect(find("setup-redesign-notice"), "not reopened in replacing mode").toBeTruthy();

    await act(async () => {
      (find("setup-skip") as HTMLElement).click();
    });

    expect(dialog()).toBeNull();
    expect(setupRedesign(SCOPE), "skip should cancel the owed redesign").toBe(false);
  });

  it("clears the redesign debt when the replacement is designed, so a reload cannot reopen it", async () => {
    // A designed replacement pays the redesign debt: the fallback team the debt
    // named has been swept and replaced, so a reload on the completion screen
    // must not reopen redesign over the new team. The whole record is cleared —
    // skipped and resuming included — exactly as completing setup would.
    const roster: TeamMemberDto[] = [...BASELINE];
    const removed: string[] = [];
    const client = {
      scopeFor: () => "/api/v1/companies/acme",
      listTeam: async () => [...roster],
      get: async (path: string) =>
        path.endsWith("/auth/me") ? { role: "admin" } : { cognition: "echo" },
      post: async () => ({
        agents: [{ name: "Ada", role: "Operations", description: "Runs the desk." }],
        template: "ecommerce",
        source: "model",
      }),
      addTeamMember: async () => {
        const member = { id: "ada", role: "Operations", inboxEnabled: false } as TeamMemberDto;
        roster.push(member);
        return member;
      },
      removeTeamMember: async (id: string) => {
        removed.push(id);
      },
    } as unknown as OpenCompanyClient;

    // Seed the debt the operator is owed: a fallback pass shipped f1 and they
    // left to wire a model, naming that row as the replacement's boundary.
    roster.push({ id: "f1", role: "Operations", inboxEnabled: false } as TeamMemberDto);
    markSetupRedesign(SCOPE, ["f1"]);

    await mount(client);

    // The debt reopens the dialog in replacing mode, without any force flag.
    expect(dialog(), "the owed redesign did not reopen").toBeTruthy();
    expect(find("setup-redesign-notice"), "not reopened in replacing mode").toBeTruthy();

    const setField = async (testId: string, value: string) => {
      const field = document.querySelector(`[data-testid="${testId}"]`) as
        | HTMLInputElement
        | HTMLTextAreaElement
        | null;
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
      await act(async () => {
        (document.querySelector('[data-testid="setup-next"]') as HTMLElement).click();
      });
    };
    await setField("setup-field-industry", "E-commerce — homeware");
    await setField("setup-field-teamHint", "");
    await setField("setup-field-automate", "");
    for (let i = 0; i < 40 && !find("setup-finish"); i++) {
      await act(async () => {
        await new Promise((r) => setTimeout(r, 60));
      });
    }

    expect(find("setup-finish"), "designed build-out never finished").toBeTruthy();
    expect(removed, "the fallback row the debt named should be swept").toEqual(["f1"]);
    // The whole record is gone — redesign, resuming and skip all cleared.
    expect(setupRedesign(SCOPE), "the designed replacement should pay the debt").toBe(false);
    expect(setupResuming(SCOPE)).toBe(false);
  });
});
