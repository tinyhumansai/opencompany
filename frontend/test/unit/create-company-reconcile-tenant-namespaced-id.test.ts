// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { ApiError } from "@/api/types";
import type { OpenCompanyClient } from "@/api/client";
import type { CompanyStatus } from "@/api/types";
import { CreateCompanyDialog } from "@/components/create-company-dialog";

/**
 * Codex review on #1828, PR comment 3865401513: on a shared-single-DB host
 * the workload namespaces every company id server-side
 * (`AppConfig::namespaced_company_id`, `runtime/types.rs`) before storing
 * it — an ordinary create sends a bare auto id such as `acme-1234`, but the
 * host stores (and answers to) `tenant-a--acme-1234`. The ambiguous-
 * provision reconciliation added for #1828 (`create-company-reset-
 * reconcile-ambiguous-provision.test.ts`,
 * `create-company-plain-create-reconcile-and-id-conflict.test.ts`) looks the
 * bare id up directly with `client.status(autoId)`, which 404s even though
 * the company exists — because this client never learns its own tenant
 * namespace (same fact `collidesWithArchived`'s own comment relies on). The
 * dialog reported failure for a create that had, in fact, fully succeeded,
 * and a retry could provision a second company under a second id.
 *
 * The fix falls back to `client.listCompanies()`, which answers with every
 * company's real id — namespaced or not — and matches either the exact
 * bare id (unnamespaced hosts) or its namespaced tail (`<tenant>--<bare
 * id>`).
 */

type ProvisionBody = { manifest_toml: string; id?: string };

function stubClient(opts: {
  provisionCompany: ReturnType<typeof vi.fn<(body: ProvisionBody) => Promise<CompanyStatus>>>;
  status: ReturnType<typeof vi.fn<(company?: string | null) => Promise<CompanyStatus>>>;
  listCompanies: ReturnType<typeof vi.fn<() => Promise<CompanyStatus[]>>>;
}) {
  return {
    carriesPlatformBearer: true,
    provisioningInfo: vi.fn(() => Promise.resolve({ auth_mode: "email", wallets_required: false })),
    lifecycle: vi.fn(() => Promise.resolve({ id: "acme" } as unknown as CompanyStatus)),
    provisionCompany: opts.provisionCompany,
    status: opts.status,
    listCompanies: opts.listCompanies,
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

describe("a create whose provisioned id lands namespaced on a shared-single-DB host", () => {
  it("reconciles via listCompanies when the bare-id status lookup 404s", async () => {
    const provisionCompany = vi.fn((_body: ProvisionBody) =>
      Promise.reject(new ApiError(0, "network_error", "network error", true)),
    );
    // The bare id this client asked for was never stored under that exact
    // key — the host namespaced it.
    const status = vi.fn(() =>
      Promise.reject(new ApiError(404, "company_not_found", "company not found", true)),
    );
    let namespacedId = "";
    const listCompanies = vi.fn(() =>
      Promise.resolve([{ id: namespacedId, name: "Acme Robotics", lifecycle: "running", pending_approvals: 0 } as unknown as CompanyStatus]),
    );
    await open(stubClient({ provisionCompany, status, listCompanies }));

    await setName("Acme Robotics");
    await fillAdminEmail();

    // We only learn the bare id `submit` generated once it calls
    // `provisionCompany`; seed the listing's namespaced answer from it via a
    // one-shot mock implementation triggered by that same call.
    provisionCompany.mockImplementationOnce((body: ProvisionBody) => {
      namespacedId = `tenant-a--${body.id}`;
      return Promise.reject(new ApiError(0, "network_error", "network error", true));
    });

    await submit();

    const sentId = provisionCompany.mock.calls[0]![0].id!;
    expect(status).toHaveBeenCalledWith(sentId);
    expect(listCompanies).toHaveBeenCalledTimes(1);
    expect(onCreated).toHaveBeenCalledTimes(1);
    expect(onCreated.mock.calls[0]![0]).toEqual({
      id: `tenant-a--${sentId}`,
      name: "Acme Robotics",
      lifecycle: "running",
      pending_approvals: 0,
    });
    expect(document.querySelector('[data-testid="create-company-error"]')).toBeFalsy();
  });

  it("still reports failure when no listed company matches either id form", async () => {
    const provisionCompany = vi.fn((_body: ProvisionBody) =>
      Promise.reject(new ApiError(0, "network_error", "network error", true)),
    );
    const status = vi.fn(() =>
      Promise.reject(new ApiError(404, "company_not_found", "company not found", true)),
    );
    const listCompanies = vi.fn(() =>
      Promise.resolve([
        { id: "tenant-a--some-other-company", name: "Other", lifecycle: "running", pending_approvals: 0 } as unknown as CompanyStatus,
      ]),
    );
    await open(stubClient({ provisionCompany, status, listCompanies }));

    await setName("Acme Robotics");
    await fillAdminEmail();
    await submit();

    expect(listCompanies).toHaveBeenCalledTimes(1);
    expect(onCreated).not.toHaveBeenCalled();
    expect(document.querySelector('[data-testid="create-company-error"]')).toBeTruthy();
  });
});
