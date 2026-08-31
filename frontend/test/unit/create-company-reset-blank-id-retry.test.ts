// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { ApiError } from "@/api/types";
import type { OpenCompanyClient } from "@/api/client";
import type { CompanyStatus } from "@/api/types";
import { CreateCompanyDialog } from "@/components/create-company-dialog";

/**
 * Codex review on #1828 (PR comment 3865689239): the Advanced id field
 * prefills with `resetReplacementId(request.company)` when a reset dialog
 * opens, but the field is editable and the dialog explicitly permits
 * clearing it back to blank. `submit` only persisted a self-generated id
 * into `explicitId` state for `request.kind === "create"` — the sibling fix
 * in `create-company-retry-preserves-generated-id.test.ts` (comment
 * 3865401532) never covered a reset whose field was blanked out, because at
 * the time that fix landed a reset's id was believed to always already be in
 * state from the open-time `useEffect`.
 *
 * That's true only until the operator clears the field. From then on, every
 * submit with a blank field recomputes `id` fresh via `resetReplacementId`,
 * which is random-suffixed — so a retry after an ambiguous outcome (host
 * provisioned the replacement, but the response and the reconciliation
 * lookup both failed) sends a *different* id than the one that may have
 * already landed, provisioning a second replacement instead of reconciling
 * the first.
 */

type ProvisionBody = { manifest_toml: string; id?: string };

function stubClient(opts: {
  provisionCompany: ReturnType<typeof vi.fn<(body: ProvisionBody) => Promise<CompanyStatus>>>;
  status: ReturnType<typeof vi.fn<(company?: string | null) => Promise<CompanyStatus>>>;
}) {
  return {
    carriesPlatformBearer: true,
    provisioningInfo: vi.fn(() => Promise.resolve({ auth_mode: "email", wallets_required: false })),
    lifecycle: vi.fn(() => Promise.resolve()),
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
  ).find((b) => b.textContent?.trim().startsWith("Archive & start clean"));
  expect(match, 'no button labeled "Archive & start clean"').toBeTruthy();
  return match as HTMLButtonElement;
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

async function fillAdminEmail(value = "ceo@acme.test") {
  const input = document.querySelector<HTMLInputElement>("#create-company-admin");
  expect(input, "no admin-email field").toBeTruthy();
  const setter = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")!.set!;
  await act(async () => {
    setter.call(input, value);
    input!.dispatchEvent(new Event("input", { bubbles: true }));
  });
}

async function clearAdvancedId() {
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
    setter.call(idInput, "");
    idInput!.dispatchEvent(new Event("input", { bubbles: true }));
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

describe("a reset retried after clearing the Advanced id back to blank", () => {
  it("sends the exact same self-generated replacement id on the retry", async () => {
    // Every attempt is ambiguous (network_error) AND every reconciliation
    // lookup also comes up empty — the dialog reports failure and stays
    // open, which is the only situation where an operator retries at all.
    const provisionCompany = vi.fn((_body: ProvisionBody) =>
      Promise.reject(new ApiError(0, "network_error", "network error", true)),
    );
    const status = vi.fn(() =>
      Promise.reject(new ApiError(404, "company_not_found", "company not found", true)),
    );
    await open(stubClient({ provisionCompany, status }));

    await clearAdvancedId();
    await fillAdminEmail();

    await submit();
    expect(provisionCompany).toHaveBeenCalledTimes(1);
    const firstId = provisionCompany.mock.calls[0]![0].id;
    expect(firstId).toMatch(/^acme-/);
    expect(firstId).not.toBe("acme");
    expect(document.querySelector('[data-testid="create-company-error"]')).toBeTruthy();

    // Retry — field stays blank, same submit button. The archive leg does
    // not fire again: `archived` is already true from the first attempt.
    await submit();
    expect(provisionCompany).toHaveBeenCalledTimes(2);
    const secondId = provisionCompany.mock.calls[1]![0].id;

    expect(secondId).toBe(firstId);
  });

  /**
   * Codex review on #1828, PR comment 3865803917: reusing the same id on
   * retry (the test above) is necessary but not sufficient. The blank-reset
   * fix persists the self-generated replacement id into `explicitId` state,
   * but the operator's earlier clearing of the field already set
   * `idTouched` true — and nothing reset it back when the fallback id was
   * cached. On the *next* submit, `explicitId` now holds that cached,
   * nonblank, self-generated value, so `selfGenerated` reads `idTouched` and
   * (without the fix) wrongly concludes the id is operator-typed, zeroing
   * `autoId`. That disables the reconciliation lookup exactly when it's
   * needed most: a retry that lands on `company_exists` because the FIRST
   * attempt actually landed on the host and only its reply was lost. Without
   * reconciliation the operator sees a bare refusal instead of being carried
   * into the company that already exists under the id they never even
   * typed.
   */
  it("reconciles a retry that lands on company_exists using the cached self-generated id", async () => {
    const provisionCompany = vi.fn<(body: ProvisionBody) => Promise<CompanyStatus>>();
    // First attempt: ambiguous network failure, and the immediate
    // reconciliation lookup also comes up empty — the dialog reports
    // failure and stays open, caching the self-generated id it sent.
    provisionCompany.mockImplementationOnce(() =>
      Promise.reject(new ApiError(0, "network_error", "network error", true)),
    );
    // Retry, same id: the host recognizes it as already provisioned — this
    // is THIS request's own first attempt having landed, not a genuine
    // collision with someone else's company.
    provisionCompany.mockImplementationOnce(() =>
      Promise.reject(new ApiError(409, "company_exists", "company already exists", true)),
    );

    const reconciled = { id: "acme-reconciled", lifecycle: "running" } as unknown as CompanyStatus;
    let statusCalls = 0;
    const status = vi.fn((_company?: string | null) => {
      statusCalls += 1;
      // Call 1: the reconciliation lookup right after the first (network_error)
      // attempt — genuinely not there yet.
      if (statusCalls === 1) {
        return Promise.reject(new ApiError(404, "company_not_found", "company not found", true));
      }
      // Call 2: the reconciliation lookup after the retry's company_exists —
      // the company is there under the id this client itself generated.
      return Promise.resolve(reconciled);
    });

    await open(stubClient({ provisionCompany, status }));
    await clearAdvancedId();
    await fillAdminEmail();

    await submit();
    const firstId = provisionCompany.mock.calls[0]![0].id;
    expect(document.querySelector('[data-testid="create-company-error"]')).toBeTruthy();

    await submit();
    expect(provisionCompany).toHaveBeenCalledTimes(2);
    expect(provisionCompany.mock.calls[1]![0].id).toBe(firstId);

    // The reconciliation lookup must have fired for the SAME cached id —
    // proving the retry still recognised it as self-generated rather than
    // treating it as an operator-typed id (which reconciliation must never
    // look up).
    expect(status).toHaveBeenLastCalledWith(firstId);
    expect(onCreated).toHaveBeenCalledWith(reconciled);
  });
});
