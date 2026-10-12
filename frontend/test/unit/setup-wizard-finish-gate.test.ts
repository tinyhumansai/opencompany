// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it } from "vitest";

import type { OpenCompanyClient } from "@/api/client";
import type { SetupStatus } from "@/api/setup";
import { SetupWizard } from "@/views/setup/SetupWizard";

/**
 * The zero-company dead end (CodeRabbit review on #908): a host with no
 * companies must not be able to finish setup without a company to finish
 * *into*, because that is exactly the "no companies running, no way back into
 * setup" dead end the flow exists to remove.
 *
 * The **condition** changed when the two setups merged — it used to be "a
 * template was picked", and is now "a roster was designed and reviewed" —
 * but the invariant is the same one and is why this file still exists.
 *
 * A pure test cannot reach this — the claim is about a *button's disabled
 * state* changing as the operator moves through the wizard, which only
 * exists once the component is mounted and rendering. Same earned exception
 * as `provider-detail-render` and `working-indicator`.
 */

function status(over: Partial<SetupStatus> = {}): SetupStatus {
  return {
    complete: false,
    config_path: "/data/config.toml",
    fields: [],
    templates: [],
    auth_modes: ["email"],
    build: {
      acp_in_build: false,
      acp_transport_mounted: false,
      mcp_in_build: false,
      harness_in_build: false,
      oauth_in_build: false,
    },
    companies: [],
    inference: { ready: false, provider: null, base_url: null },
    mail: { wired: false, echoes_code: false },
    ...over,
  };
}

/**
 * `over.post` answers the **connection test** only.
 *
 * Routed by path rather than replacing `post` wholesale, because the wizard
 * makes three different calls through it — the test, the roster design, and the
 * apply — and a blanket override would silently change what the other two see.
 */
function clientWith(
  s: SetupStatus,
  over: { post?: (path: string, body: unknown) => Promise<unknown> } = {},
): OpenCompanyClient {
  return {
    get: async () => s,
    post: async (path: string, body: unknown) => {
      if (over.post && path.includes("/inference/test")) return over.post(path, body);
      return {
        complete: true,
        config_path: s.config_path,
        restart_required: [],
        seeded_company: null,
      };
    },
  } as unknown as OpenCompanyClient;
}

let container: HTMLDivElement;
let root: Root;

async function show(client: OpenCompanyClient) {
  await act(async () => {
    root.render(createElement(SetupWizard, { client, onDone: () => {} }));
  });
}

function button(label: string): HTMLButtonElement {
  const buttons = Array.from(container.querySelectorAll("button"));
  // The advance button is labelled "Looks good" on the Advanced step, because
  // there is nothing to answer there — same control, different word.
  const wanted = label === "Next" ? ["Next", "Looks good"] : [label];
  const match = buttons.find((b) => wanted.includes(b.textContent?.trim() ?? ""));
  expect(match, `no button labeled "${label}"`).toBeTruthy();
  return match as HTMLButtonElement;
}

/** Types into a step's field, so a required one can be left. */
async function fill(testId: string, value: string) {
  const field = container.querySelector(`[data-testid="${testId}"]`) as
    | HTMLInputElement
    | HTMLTextAreaElement;
  expect(field, `no field ${testId}`).toBeTruthy();
  await act(async () => {
    const setter = Object.getOwnPropertyDescriptor(
      field instanceof HTMLTextAreaElement
        ? HTMLTextAreaElement.prototype
        : HTMLInputElement.prototype,
      "value",
    )!.set!;
    setter.call(field, value);
    field.dispatchEvent(new Event("input", { bubbles: true }));
  });
}

const next = async () =>
  act(async () => {
    button("Next").click();
  });

/**
 * Gets past step 0 onto step 1, and is a no-op once already there.
 *
 * The flow opens on the setup-way choice, and the add-provider sequence sits
 * behind "Set it up yourself".
 */
async function chooseSelfManaged() {
  const option = container.querySelector('[data-testid="setup-way-self-managed"]') as
    | HTMLElement
    | null;
  if (!option) return;
  await act(async () => {
    option.click();
  });
  await next();
}

/**
 * Onto the managed step 1, which is the branch a credential is typed and proved
 * on — and the only one this file's gate assertions are about.
 */
async function chooseManaged() {
  const option = container.querySelector('[data-testid="setup-way-managed"]') as
    | HTMLElement
    | null;
  if (!option) return;
  await act(async () => {
    option.click();
  });
  await next();
}

/**
 * Leaves step 1 with nothing connected.
 *
 * The self-managed branch's two connections are both optional, so this is the
 * whole of skipping it — and Next is not gated there, which is its own
 * assertion below.
 */
async function skipConnect() {
  await chooseSelfManaged();
  await next(); // -> business
}

/** step 1 -> business -> sign-in -> account -> review. */
async function goToReview() {
  await skipConnect();
  await fill("setup-field-industry", "E-commerce — homeware");
  await next(); // -> sign-in
  // Left as it stands: the host default is email sign-in, which is the case
  // this file is about — an operator who will have to sign in and so needs an
  // address on the next screen.
  await next(); // -> account
  // The address is required on any host that asks people to sign in — leaving
  // it blank holds the wizard here, which is its own assertion below.
  await fill("setup-field-email", "ada@example.com");
  await next(); // -> review
  // Entering Review kicks off the design call. Let it settle, or the assertions
  // below run against the spinner rather than the outcome.
  await act(async () => {
    await new Promise((resolve) => setTimeout(resolve, 0));
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

const finishButton = () =>
  container.querySelector('[data-testid="setup-finish"]') as HTMLButtonElement | null;

describe("finishing setup with no companies on the host", () => {
  it("offers the shipped company templates as a dropdown", async () => {
    await show(
      clientWith(
        status({
          templates: [
            { id: "software_company", name: "Agentic Software Company", agent_count: 5, output: "Software" },
            { id: "law_firm", name: "Agentic Law Firm", agent_count: 4, output: "Legal work" },
          ],
        }),
      ),
    );
    await skipConnect();

    const picker = container.querySelector(
      '[data-testid="setup-field-template"]',
    ) as HTMLSelectElement;
    expect(picker).toBeTruthy();
    expect(Array.from(picker.options).map((option) => option.textContent)).toContain(
      "Agentic Software Company (5 agents)",
    );

    await act(async () => {
      picker.value = "software_company";
      picker.dispatchEvent(new Event("change", { bubbles: true }));
    });
    expect(
      (container.querySelector('[data-testid="setup-field-industry"]') as HTMLInputElement).value,
    ).toBe("Agentic Software Company");
  });

  /**
   * The design call fails in this environment (no host behind the client), so
   * Review renders its error rather than a roster — which is precisely the
   * state that must not be finishable: there is no team to build, and applying
   * would leave a configured instance with nothing to sign in to.
   */
  it("refuses to finish when no team was designed", async () => {
    await show(clientWith(status()));
    await goToReview();

    expect(container.querySelector('[data-testid="setup-design-error"]')).toBeTruthy();
    expect(finishButton()?.disabled).toBe(true);
  });

  /**
   * The first question is the only required one, and it gates leaving step one
   * — so an operator cannot skip past every screen into a company with no
   * description behind it.
   */
  it("will not leave the first question empty", async () => {
    await show(clientWith(status()));
    await skipConnect();

    await act(async () => {
      button("Next").click();
    });
    expect(container.querySelector('[data-testid="setup-problem"]')).toBeTruthy();
    // Still on the first question.
    expect(
      container.querySelector('[data-testid="setup-field-industry"]'),
    ).toBeTruthy();
  });

  /**
   * The other half of the dead end, and the one that was actually reachable in
   * shipped code: an operator who finishes setup on an email-sign-in host
   * without an address can then sign in as nobody, because no shipped template
   * invites anyone.
   */
  it("will not pass the email step on a host that asks people to sign in", async () => {
    await show(clientWith(status()));
    await skipConnect();
    await fill("setup-field-industry", "E-commerce — homeware");
    await next(); // -> sign-in
    await next(); // -> account, because the mode above asks people to sign in

    // Pressing on with no address must not leave this step.
    await next();
    expect(container.querySelector('[data-testid="setup-problem"]')).toBeTruthy();
    expect(container.querySelector('[data-testid="setup-field-email"]')).toBeTruthy();
  });

  /**
   * A host that already serves a company is not at risk of the dead end: there
   * is somewhere to sign in to regardless of what this flow does, so an
   * operator reconfiguring one may finish without designing anything.
   */
  it("does not gate finishing when the host already has a company", async () => {
    await show(clientWith(status({ companies: ["acme"] })));
    await goToReview();

    expect(finishButton()?.disabled).toBe(false);
  });

  // -------------------------------------------------------------------------
  // The model gate (step one)
  // -------------------------------------------------------------------------

  /**
   * The reason this step moved to the front.
   *
   * The design pass is silent about credentials — it falls back to a curated
   * team on any failure — so an untested key produces a *plausible* company
   * rather than an error, and the operator finds out several screens later, if
   * at all. Untested therefore holds the flow here.
   */
  it("will not pass the managed step on an untested connection", async () => {
    await show(clientWith(status()));
    await chooseManaged();

    await act(async () => {
      button("Next").click();
    });
    expect(container.querySelector('[data-testid="setup-problem"]')).toBeTruthy();
    // Still on the model step, not the first question.
    expect(container.querySelector('[data-testid="setup-field-key"]')).toBeTruthy();
    expect(container.querySelector('[data-testid="setup-field-industry"]')).toBeNull();
  });

  /**
   * Nobody gets stuck (decision D3). A hosted operator has no key of their own
   * and no way to get one, so a credential must never be the single thing that
   * traps them — the curated team exists for exactly this path.
   */
  it("lets an operator continue without a model, explicitly", async () => {
    await show(clientWith(status()));
    await skipConnect();

    expect(container.querySelector('[data-testid="setup-field-industry"]')).toBeTruthy();
  });

  it("lets a managed operator explicitly continue without a model", async () => {
    await show(clientWith(status()));
    await chooseManaged();
    await next();

    expect(container.querySelector('[data-testid="setup-problem"]')).toBeTruthy();
    await act(async () => {
      button("Continue without a model").click();
    });

    expect(container.querySelector('[data-testid="setup-field-industry"]')).toBeTruthy();
    expect(container.querySelector('[data-testid="setup-field-key"]')).toBeNull();
    expect(container.querySelector('[data-testid="setup-problem"]')).toBeNull();
  });

  it.each(["failed", "pending"])(
    "discards the managed credential when continuing after a %s probe",
    async (probe) => {
      let resolveProbe!: (value: unknown) => void;
      const response = new Promise<unknown>((resolve) => {
        resolveProbe = resolve;
      });
      const client = clientWith(status(), { post: async () => response });
      const requests: { path: string; body: unknown }[] = [];
      const post = client.post.bind(client);
      client.post = async (path, body, ...rest) => {
        requests.push({ path, body });
        return post(path, body, ...rest);
      };
      await show(client);
      await chooseManaged();
      await fill("setup-field-key", "discard-this-key");
      await act(async () => {
        button("Test connection").click();
      });
      if (probe === "failed") {
        await act(async () => {
          resolveProbe({ ok: false, error: "Rejected" });
        });
      }
      await act(async () => {
        button("Continue without a model").click();
      });
      if (probe === "pending") {
        await act(async () => {
          resolveProbe({ ok: true, baseUrl: "https://example.test/v1" });
        });
      }

      expect(container.querySelector('[data-testid="setup-field-industry"]')).toBeTruthy();
      expect(container.querySelector('[data-testid="setup-field-teamHint"]')).toBeNull();
      expect(container.querySelector('[data-testid="setup-field-automate"]')).toBeNull();
      await fill("setup-field-industry", "Homeware");
      await next();
      await next();
      await fill("setup-field-email", "ada@example.com");
      await next();
      const design = requests.find(({ path }) => path.includes("/roster"));
      expect(design?.body).toMatchObject({ inferenceKey: null, forceCurated: true });
    },
  );

  it.each(["success", "failure", "rejection"])(
    "ignores a late host probe %s after explicitly continuing without a model",
    async (outcome) => {
      let resolveProbe!: (value: unknown) => void;
      let rejectProbe!: (error: Error) => void;
      const response = new Promise<unknown>((resolve, reject) => {
        resolveProbe = resolve;
        rejectProbe = reject;
      });
      await show(
        clientWith(
          status({ inference: { ready: true, provider: "managed", base_url: null } }),
          { post: async () => response },
        ),
      );
      await chooseManaged();
      await act(async () => {
        button("Continue without a model").click();
      });
      await act(async () => {
        if (outcome === "rejection") rejectProbe(new Error("Host unreachable"));
        else resolveProbe({ ok: outcome === "success", error: "Host rejected" });
      });

      expect(container.querySelector('[data-testid="setup-field-industry"]')).toBeTruthy();
      expect(container.querySelector('[data-testid="setup-field-teamHint"]')).toBeNull();
      expect(container.querySelector('[data-testid="setup-field-automate"]')).toBeNull();
      expect(container.querySelector('[data-testid="setup-problem"]')).toBeNull();
    },
  );

  /**
   * A failed test is not a passed one. The step reports the reason and still
   * holds, because "we could not reach that" is exactly when carrying on
   * silently would produce the wrong company.
   */
  it("holds, and says why, when the connection fails", async () => {
    await show(
      clientWith(status(), {
        post: async () => ({
          ok: false,
          baseUrl: "https://api.tinyhumans.ai/openai/v1",
          error: "That key was rejected by the provider.",
        }),
      }),
    );

    await chooseManaged();
    await fill("setup-field-key", "rejected-key");
    await act(async () => {
      (
        container.querySelector('[data-testid="setup-test-connection"]') as HTMLElement
      ).click();
    });
    await act(async () => {
      await new Promise((resolve) => setTimeout(resolve, 0));
    });

    const failure = container.querySelector('[data-testid="setup-test-failed"]');
    expect(failure?.textContent).toContain("rejected by the provider");
    await act(async () => {
      button("Next").click();
    });
    expect(container.querySelector('[data-testid="setup-field-industry"]')).toBeNull();
  });

  /**
   * A passing test releases the gate, and names the endpoint it reached — a
   * tick earned against the default endpoint, on a host meant to point
   * elsewhere, is a wrong answer the operator watched us produce.
   */
  it("releases the gate on a passing test, naming the endpoint", async () => {
    await show(
      clientWith(status(), {
        post: async () => ({ ok: true, baseUrl: "https://example.test/v1" }),
      }),
    );

    await chooseManaged();
    await fill("setup-field-key", "working-key");
    await act(async () => {
      (
        container.querySelector('[data-testid="setup-test-connection"]') as HTMLElement
      ).click();
    });
    await act(async () => {
      await new Promise((resolve) => setTimeout(resolve, 0));
    });

    expect(
      container.querySelector('[data-testid="setup-test-ok"]')?.textContent,
    ).toContain("https://example.test/v1");
    expect(container.textContent).not.toContain("Continue without a model");
    await next();
    expect(container.querySelector('[data-testid="setup-field-industry"]')).toBeTruthy();
  });

  it("requires a key before testing", async () => {
    let requests = 0;
    await show(
      clientWith(status(), {
        post: async () => {
          requests += 1;
          return { ok: true, baseUrl: "https://example.test/v1" };
        },
      }),
    );

    await chooseManaged();
    expect(button("Test connection").disabled).toBe(true);
    await act(async () => {
      button("Test connection").click();
    });
    expect(requests).toBe(0);

    await fill("setup-field-key", "working-key");
    expect(button("Test connection").disabled).toBe(false);
    await act(async () => {
      button("Test connection").click();
      await new Promise((resolve) => setTimeout(resolve, 0));
    });
    expect(requests).toBe(1);
  });


  /**
   * A regression guard for a bug that reached a screenshot: `\u2014` written
   * into JSX text, where nothing interprets it, so the operator read a literal
   * escape sequence where an em dash belonged.
   *
   * Asserted over the rendered text rather than the source, because that is
   * where the defect was visible and where any future one will be.
   */
  it("renders no un-interpreted escape sequences", async () => {
    await show(clientWith(status()));
    const text = container.textContent ?? "";
    expect(text).not.toMatch(/\\u[0-9a-fA-F]{4}/);
    expect(text.length).toBeGreaterThan(0);
  });

});

/*
 * The hosted-tenant cases that used to live here — the host's own key stated as
 * settled, the "use my own" override, and the apply that stored no provider —
 * are in `setup-wizard-hosted-model.test.ts` now. A host reporting
 * `inference.ready` does not show the model step at all, so there is no screen
 * left for them to mount.
 */
