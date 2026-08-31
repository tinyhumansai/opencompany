// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { ApiError } from "@/api/types";
import type { OpenCompanyClient } from "@/api/client";
import type { CompanyStatus } from "@/api/types";
import { CreateCompanyDialog } from "@/components/create-company-dialog";

/**
 * Codex review on #1828 (PR comment 3863028397): a provisioning request is
 * ambiguous by construction on a dropped connection — `ApiClient.request`
 * throws the same `network_error` whether the host never saw it or it
 * provisioned the company and only the reply was lost. The old code treated
 * that as a fresh failure. On a reset the auto-generated replacement id
 * (`resetReplacementId`) is what gets retried, so a retry after a lost reply
 * lands on `company_exists` — the host naming the id it just created for us a
 * moment earlier — and the old copy for that code ("choose a different
 * name") sent the operator looking for a naming collision that was not
 * there, with the old company already archived and no way back into the new
 * one from this dialog.
 *
 * These drive the dialog exactly like
 * `create-company-reset-archive-not-found.test.ts` drives the archive leg's
 * version of the same ambiguity, and additionally cover the guard that keeps
 * this reconciliation off an operator-typed id.
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
  ).find((b) => b.textContent?.trim().startsWith("Archive & start clean"));
  expect(match, 'no button labeled "Archive & start clean"').toBeTruthy();
  return match as HTMLButtonElement;
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

async function open(client: OpenCompanyClient) {
  await act(async () => {
    root.render(
      createElement(CreateCompanyDialog, {
        client,
        request: { kind: "reset", company: "acme", name: "Acme Robotics" },
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

// Required after codex review comment 3864885200 — unrelated to what this
// file pins (ambiguous-provision reconciliation), so every submit needs one
// filled in first or validation blocks it before the archive leg even runs.
async function fillAdminEmail(value = "ceo@acme.test") {
  const input = document.querySelector<HTMLInputElement>("#create-company-admin");
  expect(input, "no admin-email field").toBeTruthy();
  const setter = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")!.set!;
  await act(async () => {
    setter.call(input, value);
    input!.dispatchEvent(new Event("input", { bubbles: true }));
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

describe("a reset whose provision answers ambiguously", () => {
  it("reconciles a network_error via status and treats it as success", async () => {
    const provisionCompany = vi.fn((_body: ProvisionBody) =>
      Promise.reject(new ApiError(0, "network_error", "network error", true)),
    );
    const status = vi.fn((company?: string | null) =>
      Promise.resolve({ id: company } as unknown as CompanyStatus),
    );
    await open(stubClient({ provisionCompany, status }));

    await fillAdminEmail();
    await submit();

    expect(provisionCompany).toHaveBeenCalledTimes(1);
    const sentId = provisionCompany.mock.calls[0]![0].id;
    expect(sentId).toMatch(/^acme-/);
    // Reconciled against the exact id the (only) provision attempt sent.
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

    await fillAdminEmail();
    await submit();

    expect(onCreated).toHaveBeenCalledTimes(1);
    expect(document.querySelector('[data-testid="create-company-error"]')).toBeFalsy();
  });

  it("still reports failure when the reconciliation lookup also comes up empty", async () => {
    const provisionCompany = vi.fn((_body: ProvisionBody) =>
      Promise.reject(new ApiError(0, "network_error", "network error", true)),
    );
    const status = vi.fn(() =>
      Promise.reject(new ApiError(404, "company_not_found", "company not found", true)),
    );
    await open(stubClient({ provisionCompany, status }));

    await fillAdminEmail();
    await submit();

    expect(onCreated).not.toHaveBeenCalled();
    const alert = document.querySelector('[data-testid="create-company-error"]');
    expect(alert).toBeTruthy();
    expect(alert!.textContent).toContain("Archived Acme Robotics");
  });

  it("never reconciles an operator-typed id — a real collision still reports as one", async () => {
    const provisionCompany = vi.fn((_body: ProvisionBody) =>
      Promise.reject(new ApiError(409, "company_exists", "company already exists", true)),
    );
    const status = vi.fn(() =>
      Promise.resolve({ id: "someone-elses-company" } as unknown as CompanyStatus),
    );
    await open(stubClient({ provisionCompany, status }));

    await setExplicitId("someone-elses-company");
    await fillAdminEmail();
    await submit();

    // No reconciliation lookup for a typed id — the console must never
    // silently switch into a company it did not just create.
    expect(status).not.toHaveBeenCalled();
    expect(onCreated).not.toHaveBeenCalled();
    expect(document.querySelector('[data-testid="create-company-error"]')).toBeTruthy();
  });
});
