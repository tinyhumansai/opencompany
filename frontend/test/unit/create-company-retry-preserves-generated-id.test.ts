// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { ApiError } from "@/api/types";
import type { OpenCompanyClient } from "@/api/client";
import type { CompanyStatus } from "@/api/types";
import { CreateCompanyDialog } from "@/components/create-company-dialog";

/**
 * Codex review on #1828, PR comment 3865401532: a plain "New company"
 * request leaves the Advanced id field blank on open (unlike a reset, whose
 * `resetReplacementId` default is seeded into `explicitId` — and therefore
 * into component state — the moment the dialog opens). `submit` computed a
 * plain create's auto id inline with `autoCompanyId(trimmedName)`, called
 * fresh on every invocation, so every *retry* minted a brand-new random
 * suffix.
 *
 * That matters exactly when a retry is needed: a default create that
 * succeeded on the host but lost its response, where the immediate
 * ambiguous-provision reconciliation (`create-company-plain-create-
 * reconcile-and-id-conflict.test.ts`) *also* fails transiently, leaves the
 * dialog open with no success reported. The operator's only option is
 * retry — and the old code sent that retry under a different id, so instead
 * of reconciling the first company it provisioned a second one, consuming
 * quota and leaving an unintended duplicate.
 *
 * Sibling-path check: a reset survives a retry unchanged *as long as the
 * operator never touches the Advanced field* — its id is seeded into
 * `explicitId` state at open time and is never regenerated inline the way a
 * plain create's was. A reset whose field gets cleared back to blank has the
 * same bug this test guards, for the same reason; see
 * `create-company-reset-blank-id-retry.test.ts` (issue #1828 comment
 * 3865689239) for that case and the fix that also covers it.
 */

type ProvisionBody = { manifest_toml: string; id?: string };

function stubClient(opts: {
  provisionCompany: ReturnType<typeof vi.fn<(body: ProvisionBody) => Promise<CompanyStatus>>>;
  status: ReturnType<typeof vi.fn<(company?: string | null) => Promise<CompanyStatus>>>;
}) {
  return {
    carriesPlatformBearer: true,
    provisioningInfo: vi.fn(() => Promise.resolve({ auth_mode: "email", wallets_required: false })),
    lifecycle: vi.fn(() => Promise.resolve({ id: "acme" } as unknown as CompanyStatus)),
    provisionCompany: opts.provisionCompany,
    status: opts.status,
    listCompanies: vi.fn(() => Promise.resolve([])),
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

describe("a plain create retried after an unresolved ambiguous provision", () => {
  it("sends the exact same self-generated id on the retry", async () => {
    // Every attempt is ambiguous (network_error) AND every reconciliation
    // lookup also comes up empty (still in flight, or genuinely lost) — the
    // dialog reports failure and stays open, which is the only situation
    // where an operator retries at all.
    const provisionCompany = vi.fn((_body: ProvisionBody) =>
      Promise.reject(new ApiError(0, "network_error", "network error", true)),
    );
    const status = vi.fn(() =>
      Promise.reject(new ApiError(404, "company_not_found", "company not found", true)),
    );
    await open(stubClient({ provisionCompany, status }));

    await setName("Acme Robotics");
    await fillAdminEmail();

    await submit();
    expect(provisionCompany).toHaveBeenCalledTimes(1);
    const firstId = provisionCompany.mock.calls[0]![0].id;
    expect(firstId).toMatch(/^acme-robotics-/);
    expect(document.querySelector('[data-testid="create-company-error"]')).toBeTruthy();

    // Retry — no field touched, same submit button.
    await submit();
    expect(provisionCompany).toHaveBeenCalledTimes(2);
    const secondId = provisionCompany.mock.calls[1]![0].id;

    expect(secondId).toBe(firstId);
  });

  it("still lets the operator pick a fresh id explicitly, overriding the cached one", async () => {
    const provisionCompany = vi.fn((_body: ProvisionBody) =>
      Promise.reject(new ApiError(0, "network_error", "network error", true)),
    );
    const status = vi.fn(() =>
      Promise.reject(new ApiError(404, "company_not_found", "company not found", true)),
    );
    await open(stubClient({ provisionCompany, status }));

    await setName("Acme Robotics");
    await fillAdminEmail();
    await submit();
    const firstId = provisionCompany.mock.calls[0]![0].id;

    const advancedToggle = Array.from(
      document.querySelectorAll<HTMLButtonElement>('[data-slot="dialog-content"] button'),
    ).find((b) => b.textContent === "Advanced");
    expect(advancedToggle, "no Advanced toggle").toBeTruthy();
    await act(async () => {
      advancedToggle!.click();
    });
    const idInput = document.querySelector<HTMLInputElement>("#create-company-id");
    expect(idInput, "no company-id field").toBeTruthy();
    // The cached generated id is now visible in the field (issue #1828
    // comment 3865401532's fix) — the operator overwrites it explicitly.
    expect(idInput!.value).toBe(firstId);
    const setter = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")!.set!;
    await act(async () => {
      setter.call(idInput, "acme-picked-by-hand");
      idInput!.dispatchEvent(new Event("input", { bubbles: true }));
    });

    await submit();
    const secondId = provisionCompany.mock.calls[1]![0].id;
    expect(secondId).toBe("acme-picked-by-hand");
  });
});
