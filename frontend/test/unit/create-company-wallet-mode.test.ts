// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { OpenCompanyClient } from "@/api/client";
import type { CompanyStatus, ProvisioningInfo } from "@/api/types";
import { ApiError } from "@/api/types";
import {
  CreateCompanyDialog,
  type CreateCompanyRequest,
} from "@/components/create-company-dialog";

/**
 * The wallet-mode create/reset flow (issues #1914, #1894).
 *
 * The dialog reads a provisioning preflight on open. On a wallet-mode host it
 * asks for a wallet address instead of an email, and — the #1894 fix — the
 * wallet check runs in the same pre-archive validation block as the email
 * check, so a reset never archives the old company before discovering the
 * replacement has no usable admin. A preflight it cannot read refuses before
 * the destructive leg rather than archiving blind.
 */

type ProvisionBody = { manifest_toml: string; id?: string };

function stubClient(opts: {
  provisioningInfo: ReturnType<typeof vi.fn<() => Promise<ProvisioningInfo>>>;
  lifecycle?: ReturnType<typeof vi.fn>;
  provisionCompany?: ReturnType<typeof vi.fn<(body: ProvisionBody) => Promise<CompanyStatus>>>;
}) {
  return {
    carriesPlatformBearer: true,
    provisioningInfo: opts.provisioningInfo,
    lifecycle: opts.lifecycle ?? vi.fn(() => Promise.resolve()),
    provisionCompany:
      opts.provisionCompany ??
      vi.fn(() => Promise.resolve({ id: "whatever" } as unknown as CompanyStatus)),
    status: vi.fn(() => Promise.reject(new ApiError(404, "company_not_found", "gone"))),
    listCompanies: vi.fn(() => Promise.resolve([])),
  } as unknown as OpenCompanyClient;
}

const emailPreflight = () =>
  vi.fn<() => Promise<ProvisioningInfo>>(() =>
    Promise.resolve({ auth_mode: "email", wallets_required: false }),
  );
const walletPreflight = () =>
  vi.fn<() => Promise<ProvisioningInfo>>(() =>
    Promise.resolve({ auth_mode: "wallet", wallets_required: true }),
  );

let container: HTMLDivElement;
let root: Root;
let onClose: ReturnType<typeof vi.fn>;
let onCreated: ReturnType<typeof vi.fn>;

const RESET: CreateCompanyRequest = { kind: "reset", company: "acme", name: "Acme Robotics" };

function submitButton(): HTMLButtonElement {
  const match = Array.from(
    document.querySelectorAll<HTMLButtonElement>('[data-slot="dialog-content"] button'),
  ).find((b) => b.textContent?.trim().startsWith("Archive & start clean"));
  expect(match, 'no button labeled "Archive & start clean"').toBeTruthy();
  return match as HTMLButtonElement;
}

async function open(client: OpenCompanyClient, request: CreateCompanyRequest = RESET) {
  await act(async () => {
    root.render(
      createElement(CreateCompanyDialog, { client, request, onClose, onCreated }),
    );
  });
  // Let the preflight promise resolve and the mode-dependent fields render.
  await act(async () => {
    await Promise.resolve();
    await Promise.resolve();
    await Promise.resolve();
  });
}

async function setField(selector: string, value: string) {
  const input = document.querySelector<HTMLInputElement>(selector);
  expect(input, `no field ${selector}`).toBeTruthy();
  const setter = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")!.set!;
  await act(async () => {
    setter.call(input, value);
    input!.dispatchEvent(new Event("input", { bubbles: true }));
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

describe("wallet-mode create/reset", () => {
  it("shows the wallet field, not the email field, on a wallet-mode host", async () => {
    await open(stubClient({ provisioningInfo: walletPreflight() }));
    expect(document.querySelector("#create-company-wallet")).toBeTruthy();
    expect(document.querySelector("#create-company-admin")).toBeFalsy();
  });

  // The #1894 lock: an empty wallet on a wallet-mode reset must be caught
  // BEFORE the destructive archive leg — the archive mock must never fire.
  it("refuses an empty-wallet reset before archiving", async () => {
    const lifecycle = vi.fn(() => Promise.resolve());
    const provisionCompany = vi.fn((_body: ProvisionBody) =>
      Promise.resolve({ id: "acme-x" } as unknown as CompanyStatus),
    );
    await open(stubClient({ provisioningInfo: walletPreflight(), lifecycle, provisionCompany }));

    await submit();

    expect(lifecycle).not.toHaveBeenCalled();
    expect(provisionCompany).not.toHaveBeenCalled();
    expect(onCreated).not.toHaveBeenCalled();
    const error = document.querySelector('[data-testid="create-company-error"]');
    expect(error, "no error shown").toBeTruthy();
    expect(error!.textContent).toMatch(/wallet/i);
  });

  it("archives then provisions a wallet manifest on the valid path", async () => {
    const lifecycle = vi.fn(() => Promise.resolve());
    let sent: ProvisionBody | undefined;
    const provisionCompany = vi.fn((body: ProvisionBody) => {
      sent = body;
      return Promise.resolve({ id: "acme-x" } as unknown as CompanyStatus);
    });
    await open(stubClient({ provisioningInfo: walletPreflight(), lifecycle, provisionCompany }));

    await setField("#create-company-wallet", "11111111111111111111111111111111");
    await submit();

    expect(lifecycle).toHaveBeenCalledWith("archive", "acme");
    expect(provisionCompany).toHaveBeenCalledTimes(1);
    expect(sent!.manifest_toml).toContain('mode = "wallet"');
    expect(sent!.manifest_toml).toContain(
      'wallets = ["11111111111111111111111111111111"]',
    );
    expect(sent!.manifest_toml).not.toContain("admins");
    expect(onCreated).toHaveBeenCalledTimes(1);
  });

  it("leaves the email flow unchanged on an email-mode host", async () => {
    const lifecycle = vi.fn(() => Promise.resolve());
    let sent: ProvisionBody | undefined;
    const provisionCompany = vi.fn((body: ProvisionBody) => {
      sent = body;
      return Promise.resolve({ id: "acme-x" } as unknown as CompanyStatus);
    });
    await open(stubClient({ provisioningInfo: emailPreflight(), lifecycle, provisionCompany }));

    expect(document.querySelector("#create-company-admin")).toBeTruthy();
    await setField("#create-company-admin", "ceo@acme.test");
    await submit();

    expect(lifecycle).toHaveBeenCalledWith("archive", "acme");
    expect(sent!.manifest_toml).toContain('admins = ["ceo@acme.test"]');
    expect(sent!.manifest_toml).not.toContain("wallet");
    expect(onCreated).toHaveBeenCalledTimes(1);
  });

  it("refuses before archiving when the preflight itself fails", async () => {
    const lifecycle = vi.fn(() => Promise.resolve());
    const provisionCompany = vi.fn((_body: ProvisionBody) =>
      Promise.resolve({ id: "acme-x" } as unknown as CompanyStatus),
    );
    const provisioningInfo = vi.fn<() => Promise<ProvisioningInfo>>(() =>
      Promise.reject(new ApiError(0, "network_error", "cannot reach the host")),
    );
    await open(stubClient({ provisioningInfo, lifecycle, provisionCompany }));

    // Fill the (email-default) field so nothing else can be the cause of refusal.
    await setField("#create-company-admin", "ceo@acme.test");
    await submit();

    expect(lifecycle).not.toHaveBeenCalled();
    expect(provisionCompany).not.toHaveBeenCalled();
    expect(onCreated).not.toHaveBeenCalled();
    const error = document.querySelector('[data-testid="create-company-error"]');
    expect(error, "no error shown").toBeTruthy();
    expect(error!.textContent).toMatch(/sign-in mode/i);
  });
});
