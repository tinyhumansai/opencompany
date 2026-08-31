// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { ApiError } from "@/api/types";
import type { OpenCompanyClient } from "@/api/client";
import type { CompanyStatus } from "@/api/types";
import { CreateCompanyDialog } from "@/components/create-company-dialog";

/**
 * Codex review on #1828, PR comment 3874553506, `create-company-
 * dialog.tsx:442`: `register_and_report_status` on the host
 * (`server/provision.rs`) registers a company BEFORE reading back its
 * status for the response, so a transient failure in that read still
 * leaves the company live and addressable. A retry against the same id
 * then answers `company_exists` — but for an operator-typed id,
 * `submit`'s ambiguous-provision reconciliation is deliberately skipped
 * (`create-company-plain-create-reconcile-and-id-conflict.test.ts` pins
 * that skip — an operator-typed id could belong to an unrelated,
 * pre-existing company, and auto-navigating into it would be worse than
 * the error it replaces).
 *
 * That skip is correct, but it used to leave nothing else in its place:
 * the dialog closed exactly like any other ordinary refusal, and
 * `ConnectionConsole`'s roster (the picker, or the "no company" empty
 * state) never refreshed. An operator whose own request actually
 * succeeded had no way back into the company they just created — the
 * picker's stale roster didn't show it, and the error text ("already in
 * use") reads like a name they need to change, not a company they
 * already own.
 *
 * The fix reuses the exact mechanism already proven for `archiveMaybe`
 * (`create-company-reset-cancel-reports-archived.test.ts`): a
 * `createMaybe` flag, OR'd into every `onClose` call, forces the parent
 * to refresh its roster on close whenever a provisioning attempt
 * answered ambiguously and could not be automatically reconciled — so
 * the operator can find the company by hand even though this dialog
 * could not safely navigate into it itself.
 */

type ProvisionBody = { manifest_toml: string; id?: string };

function stubClient(opts: {
  provisionCompany: ReturnType<
    typeof vi.fn<(body: ProvisionBody) => Promise<CompanyStatus>>
  >;
  status?: ReturnType<
    typeof vi.fn<(company?: string | null) => Promise<CompanyStatus>>
  >;
}) {
  return {
    carriesPlatformBearer: true,
    provisioningInfo: vi.fn(() => Promise.resolve({ auth_mode: "email", wallets_required: false })),
    lifecycle: vi.fn(() =>
      Promise.resolve({ id: "acme" } as unknown as CompanyStatus),
    ),
    provisionCompany: opts.provisionCompany,
    status:
      opts.status ??
      vi.fn(() => Promise.reject(new Error("status not stubbed"))),
  } as unknown as OpenCompanyClient;
}

let container: HTMLDivElement;
let root: Root;
let onClose: ReturnType<typeof vi.fn>;
let onCreated: ReturnType<typeof vi.fn>;

function submitButton(): HTMLButtonElement {
  const match = Array.from(
    document.querySelectorAll<HTMLButtonElement>(
      '[data-slot="dialog-content"] button',
    ),
  ).find((b) => b.textContent?.trim().startsWith("Create company"));
  expect(match, 'no button labeled "Create company"').toBeTruthy();
  return match as HTMLButtonElement;
}

function cancelButton(): HTMLButtonElement {
  const match = Array.from(
    document.querySelectorAll<HTMLButtonElement>(
      '[data-slot="dialog-content"] button',
    ),
  ).find((b) => b.textContent?.trim() === "Cancel");
  expect(match, 'no button labeled "Cancel"').toBeTruthy();
  return match as HTMLButtonElement;
}

async function setName(value: string) {
  const input = document.querySelector<HTMLInputElement>(
    "#create-company-name",
  );
  expect(input, "no company-name field").toBeTruthy();
  const setter = Object.getOwnPropertyDescriptor(
    HTMLInputElement.prototype,
    "value",
  )!.set!;
  await act(async () => {
    setter.call(input, value);
    input!.dispatchEvent(new Event("input", { bubbles: true }));
  });
}

async function setExplicitId(value: string) {
  const advancedToggle = Array.from(
    document.querySelectorAll<HTMLButtonElement>(
      '[data-slot="dialog-content"] button',
    ),
  ).find((b) => b.textContent === "Advanced");
  expect(advancedToggle, "no Advanced toggle").toBeTruthy();
  await act(async () => {
    advancedToggle!.click();
  });

  const idInput =
    document.querySelector<HTMLInputElement>("#create-company-id");
  expect(idInput, "no company-id field").toBeTruthy();
  const setter = Object.getOwnPropertyDescriptor(
    HTMLInputElement.prototype,
    "value",
  )!.set!;
  await act(async () => {
    setter.call(idInput, value);
    idInput!.dispatchEvent(new Event("input", { bubbles: true }));
  });
}

async function fillAdminEmail(value = "ceo@acme.test") {
  const input = document.querySelector<HTMLInputElement>(
    "#create-company-admin",
  );
  expect(input, "no admin-email field").toBeTruthy();
  const setter = Object.getOwnPropertyDescriptor(
    HTMLInputElement.prototype,
    "value",
  )!.set!;
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

async function settle() {
  await act(async () => {
    await Promise.resolve();
    await Promise.resolve();
    await Promise.resolve();
    await Promise.resolve();
  });
}

beforeEach(() => {
  (
    globalThis as unknown as { IS_REACT_ACT_ENVIRONMENT: boolean }
  ).IS_REACT_ACT_ENVIRONMENT = true;
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

describe("a plain create whose operator-typed id hits an unreconciled ambiguous outcome", () => {
  it("reports true on Cancel so the parent refreshes its roster", async () => {
    const provisionCompany = vi.fn((_body: ProvisionBody) =>
      Promise.reject(
        new ApiError(
          409,
          "company_exists",
          "company already exists: acme-2",
          true,
        ),
      ),
    );
    await open(stubClient({ provisionCompany }));

    await setName("Acme Robotics");
    await setExplicitId("acme-2");
    await fillAdminEmail();
    await act(async () => {
      submitButton().click();
    });
    await settle();

    // Confirms this reached the unreconciled branch, not some other error
    // path: no reconciliation lookup, no navigation into the company.
    expect(onCreated).not.toHaveBeenCalled();
    const alert = document.querySelector(
      '[data-testid="create-company-error"]',
    );
    expect(alert, "no error alert rendered").toBeTruthy();

    await act(async () => {
      cancelButton().click();
    });

    // This is the bug: an operator-typed id's `company_exists` never
    // reconciles, but until now nothing told the parent the roster might
    // be stale either — `onClose` reported `false`, same as any ordinary,
    // definite refusal, even though this exact request may have already
    // created the company.
    expect(onClose).toHaveBeenCalledExactlyOnceWith(true);
  });

  it("reports true on Escape too, not just Cancel", async () => {
    const provisionCompany = vi.fn((_body: ProvisionBody) =>
      Promise.reject(
        new ApiError(
          409,
          "company_exists",
          "company already exists: acme-2",
          true,
        ),
      ),
    );
    await open(stubClient({ provisionCompany }));

    await setName("Acme Robotics");
    await setExplicitId("acme-2");
    await fillAdminEmail();
    await act(async () => {
      submitButton().click();
    });
    await settle();

    const popup = document.querySelector<HTMLElement>(
      '[data-slot="dialog-content"]',
    );
    expect(popup, "dialog popup did not render").toBeTruthy();
    await act(async () => {
      popup!.dispatchEvent(
        new KeyboardEvent("keydown", {
          key: "Escape",
          bubbles: true,
          cancelable: true,
        }),
      );
    });

    expect(onClose).toHaveBeenCalledExactlyOnceWith(true);
  });

  it("still reports false for an ordinary, unambiguous refusal", async () => {
    // A genuine, non-ambiguous failure (a quota, say) must not spuriously
    // force a refresh — `createMaybe` is scoped to
    // `wasAmbiguousProvisionOutcome`, not every error.
    const provisionCompany = vi.fn((_body: ProvisionBody) =>
      Promise.reject(
        new ApiError(
          429,
          "quota_exceeded",
          "tenant company quota of 3 reached",
          true,
        ),
      ),
    );
    await open(stubClient({ provisionCompany }));

    await setName("Acme Robotics");
    await fillAdminEmail();
    await act(async () => {
      submitButton().click();
    });
    await settle();

    expect(
      document.querySelector('[data-testid="create-company-error"]'),
    ).toBeTruthy();

    await act(async () => {
      cancelButton().click();
    });

    expect(onClose).toHaveBeenCalledExactlyOnceWith(false);
  });
});
