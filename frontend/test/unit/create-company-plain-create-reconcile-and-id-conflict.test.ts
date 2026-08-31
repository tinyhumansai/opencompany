// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { ApiError } from "@/api/types";
import type { OpenCompanyClient } from "@/api/client";
import type { CompanyStatus } from "@/api/types";
import { CreateCompanyDialog } from "@/components/create-company-dialog";

/**
 * Codex review on #1828 (PR comment 3865190498): `submit`'s ambiguous-
 * provision reconciliation (`create-company-reset-reconcile-ambiguous-
 * provision.test.ts`) only ever ran for a reset — `selfGenerated` was gated
 * on `request.kind === "reset"`. An ordinary "New company" request left with
 * the Advanced id field blank has the exact same ambiguity: a dropped
 * connection after a successful provision, or a retry landing on
 * `company_exists`, reported failure with no way to recover, and — because
 * the id the host derives from the name is deterministic — every subsequent
 * retry landed on the identical refusal. The fix generates a self-generated,
 * random-suffixed id for a plain create too (`autoCompanyId`) and reconciles
 * it the same way a reset's `resetReplacementId` already was.
 *
 * PR comment 3865190508 is the second, related finding fixed here: the
 * console-authored `company_exists` copy always said "that name already
 * exists", even when the conflict was an operator-typed id from the Advanced
 * panel (which is what the host's own `company already exists: {id}` check
 * is actually about). `describeProvisionError` now takes whether the id was
 * operator-typed and answers accordingly.
 */

type ProvisionBody = { manifest_toml: string; id?: string };

function stubClient(opts: {
  provisionCompany: ReturnType<typeof vi.fn<(body: ProvisionBody) => Promise<CompanyStatus>>>;
  status?: ReturnType<typeof vi.fn<(company?: string | null) => Promise<CompanyStatus>>>;
}) {
  return {
    carriesPlatformBearer: true,
    provisioningInfo: vi.fn(() => Promise.resolve({ auth_mode: "email", wallets_required: false })),
    lifecycle: vi.fn(() => Promise.resolve({ id: "acme" } as unknown as CompanyStatus)),
    provisionCompany: opts.provisionCompany,
    status: opts.status ?? vi.fn(() => Promise.reject(new Error("status not stubbed"))),
  } as unknown as OpenCompanyClient;
}

let container: HTMLDivElement;
let root: Root;
let onClose: ReturnType<typeof vi.fn>;
let onCreated: ReturnType<typeof vi.fn>;

function submitButton(): HTMLButtonElement {
  const match = Array.from(
    document.querySelectorAll<HTMLButtonElement>('[data-slot="dialog-content"] button'),
  ).find((b) => b.textContent?.trim().startsWith("Create company"));
  expect(match, 'no button labeled "Create company"').toBeTruthy();
  return match as HTMLButtonElement;
}

async function setName(value: string) {
  const input = document.querySelector<HTMLInputElement>("#create-company-name");
  expect(input, "no company-name field").toBeTruthy();
  const setter = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")!.set!;
  await act(async () => {
    setter.call(input, value);
    input!.dispatchEvent(new Event("input", { bubbles: true }));
  });
}

async function setExplicitId(value: string) {
  const advancedToggle = Array.from(
    document.querySelectorAll<HTMLButtonElement>('[data-slot="dialog-content"] button'),
  ).find((b) => b.textContent === "Advanced");
  expect(advancedToggle, "no Advanced toggle").toBeTruthy();
  await act(async () => {
    advancedToggle!.click();
  });

  const idInput = document.querySelector<HTMLInputElement>("#create-company-id");
  expect(idInput, "no company-id field").toBeTruthy();
  const setter = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")!.set!;
  await act(async () => {
    setter.call(idInput, value);
    idInput!.dispatchEvent(new Event("input", { bubbles: true }));
  });
}

async function fillAdminEmail(value = "ceo@acme.test") {
  const input = document.querySelector<HTMLInputElement>("#create-company-admin");
  expect(input, "no admin-email field").toBeTruthy();
  const setter = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")!.set!;
  await act(async () => {
    setter.call(input, value);
    input!.dispatchEvent(new Event("input", { bubbles: true }));
  });
}

async function open(client: OpenCompanyClient) {
  await act(async () => {
    root.render(
      createElement(CreateCompanyDialog, {
        client,
        request: { kind: "create" },
        onClose,
        onCreated,
      }),
    );
  });
}

async function submit() {
  await act(async () => {
    submitButton().click();
  });
  await act(async () => {
    await Promise.resolve();
    await Promise.resolve();
    await Promise.resolve();
    await Promise.resolve();
  });
}

beforeEach(() => {
  (globalThis as unknown as { IS_REACT_ACT_ENVIRONMENT: boolean }).IS_REACT_ACT_ENVIRONMENT =
    true;
  onClose = vi.fn();
  onCreated = vi.fn();
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
});

describe("a plain create whose provision answers ambiguously", () => {
  it("sends a self-generated id even with the Advanced id field left blank", async () => {
    const provisionCompany = vi.fn((_body: ProvisionBody) =>
      Promise.resolve({ id: "acme-robotics" } as unknown as CompanyStatus),
    );
    await open(stubClient({ provisionCompany }));

    await setName("Acme Robotics");
    await fillAdminEmail();
    await submit();

    expect(provisionCompany).toHaveBeenCalledTimes(1);
    const sentId = provisionCompany.mock.calls[0]![0].id;
    // Slugged from the name, then suffixed — never left unset for the host
    // to derive on its own, which is what made reconciliation impossible.
    expect(sentId).toMatch(/^acme-robotics-/);
  });

  it("reconciles a network_error via status and treats it as success", async () => {
    const provisionCompany = vi.fn((_body: ProvisionBody) =>
      Promise.reject(new ApiError(0, "network_error", "network error", true)),
    );
    const status = vi.fn((company?: string | null) =>
      Promise.resolve({ id: company } as unknown as CompanyStatus),
    );
    await open(stubClient({ provisionCompany, status }));

    await setName("Acme Robotics");
    await fillAdminEmail();
    await submit();

    expect(provisionCompany).toHaveBeenCalledTimes(1);
    const sentId = provisionCompany.mock.calls[0]![0].id;
    expect(status).toHaveBeenCalledWith(sentId);
    expect(onCreated).toHaveBeenCalledTimes(1);
    expect(onCreated.mock.calls[0]![0]).toEqual({ id: sentId });
    expect(document.querySelector('[data-testid="create-company-error"]')).toBeFalsy();
  });

  it("reconciles company_exists (a retry landing on our own auto id) the same way", async () => {
    const provisionCompany = vi.fn((_body: ProvisionBody) =>
      Promise.reject(new ApiError(409, "company_exists", "company already exists", true)),
    );
    const status = vi.fn((company?: string | null) =>
      Promise.resolve({ id: company } as unknown as CompanyStatus),
    );
    await open(stubClient({ provisionCompany, status }));

    await setName("Acme Robotics");
    await fillAdminEmail();
    await submit();

    expect(onCreated).toHaveBeenCalledTimes(1);
    expect(document.querySelector('[data-testid="create-company-error"]')).toBeFalsy();
  });

  it("never reconciles an operator-typed id, and blames the id, not the name", async () => {
    const provisionCompany = vi.fn((_body: ProvisionBody) =>
      Promise.reject(new ApiError(409, "company_exists", "company already exists: acme-2", true)),
    );
    const status = vi.fn(() =>
      Promise.resolve({ id: "acme-2" } as unknown as CompanyStatus),
    );
    await open(stubClient({ provisionCompany, status }));

    await setName("Acme Robotics");
    await setExplicitId("acme-2");
    await fillAdminEmail();
    await submit();

    // No reconciliation lookup for a typed id — an operator-chosen id could
    // belong to an unrelated company.
    expect(status).not.toHaveBeenCalled();
    expect(onCreated).not.toHaveBeenCalled();
    const alert = document.querySelector('[data-testid="create-company-error"]');
    expect(alert, "no error alert rendered").toBeTruthy();
    // The bug this pins: the old copy always said "name", sending the
    // operator to edit the field that was never the problem.
    expect(alert!.textContent).toContain("id");
    expect(alert!.textContent).not.toContain("name already exists");
  });

  it("still blames the name when the id was left for auto-generation", async () => {
    // A genuinely exhausted retry: reconciliation's own lookup also comes up
    // empty, so the ambiguous `company_exists` falls through to the ordinary
    // error copy — which, for a self-generated id, should still read as a
    // name collision (the only field this operator ever saw).
    const provisionCompany = vi.fn((_body: ProvisionBody) =>
      Promise.reject(new ApiError(409, "company_exists", "company already exists", true)),
    );
    const status = vi.fn(() =>
      Promise.reject(new ApiError(404, "company_not_found", "company not found", true)),
    );
    await open(stubClient({ provisionCompany, status }));

    await setName("Acme Robotics");
    await fillAdminEmail();
    await submit();

    const alert = document.querySelector('[data-testid="create-company-error"]');
    expect(alert, "no error alert rendered").toBeTruthy();
    expect(alert!.textContent).toContain("name already exists");
  });
});
