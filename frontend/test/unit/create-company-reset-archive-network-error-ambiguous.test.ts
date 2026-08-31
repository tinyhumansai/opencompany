// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { ApiError } from "@/api/types";
import type { OpenCompanyClient } from "@/api/client";
import type { CompanyStatus } from "@/api/types";
import { CreateCompanyDialog } from "@/components/create-company-dialog";

/**
 * Codex review on #1828, PR comment 3865803912: `create-company-reset-
 * archive-not-found.test.ts` covers a RETRY's archive answering
 * `company_not_found` — proof the company is already gone because an
 * earlier attempt's own archive landed and only its reply was lost. But the
 * FIRST attempt with a lost reply throws `network_error`, not
 * `company_not_found` (the id hasn't been retried against yet), and that
 * code used to fall straight into the "the archive was refused, nothing was
 * changed" branch — a claim that may be false. An operator who trusted it
 * and closed the dialog left the console showing a company that was, in
 * fact, already archived: `onClose(false)` then skips the roster refresh
 * and persisted-default cleanup a `true` would have triggered.
 *
 * These drive the dialog through the reconciliation lookup
 * (`client.status`) the fix adds for exactly this ambiguity, covering all
 * three outcomes: the lookup proves the archive landed, proves it didn't, or
 * is itself inconclusive.
 */

type ProvisionBody = { manifest_toml: string; id?: string };

function stubClient(opts: {
  lifecycle: ReturnType<typeof vi.fn>;
  status: ReturnType<typeof vi.fn<(company?: string | null) => Promise<CompanyStatus>>>;
  provisionCompany?: ReturnType<typeof vi.fn<(body: ProvisionBody) => Promise<CompanyStatus>>>;
}) {
  return {
    carriesPlatformBearer: true,
    provisioningInfo: vi.fn(() => Promise.resolve({ auth_mode: "email", wallets_required: false })),
    lifecycle: opts.lifecycle,
    status: opts.status,
    provisionCompany:
      opts.provisionCompany ??
      vi.fn(() => Promise.resolve({ id: "whatever" } as unknown as CompanyStatus)),
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
// file pins, so every submit needs one filled in first or validation blocks
// it before the archive leg even runs.
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

describe("a reset whose FIRST archive attempt drops the reply (network_error)", () => {
  it("reconciles a lookup showing the company is gone as already archived, and proceeds", async () => {
    const lifecycle = vi.fn(() =>
      Promise.reject(new ApiError(0, "network_error", "network error", true)),
    );
    // The reply was lost, but the archive itself landed — the company is
    // gone.
    const status = vi.fn(() =>
      Promise.reject(new ApiError(404, "company_not_found", "company not found: acme", true)),
    );
    const provisionCompany = vi.fn((_body: ProvisionBody) =>
      Promise.resolve({ id: "acme-x" } as unknown as CompanyStatus),
    );
    await open(stubClient({ lifecycle, status, provisionCompany }));

    await fillAdminEmail();
    await submit();

    expect(lifecycle).toHaveBeenCalledWith("archive", "acme");
    expect(status).toHaveBeenCalledWith("acme");
    // Reconciled as already-archived: proceeds to provision, no false
    // "Nothing was changed" error, and `onClose` would report `archived`.
    expect(provisionCompany).toHaveBeenCalledTimes(1);
    expect(onCreated).toHaveBeenCalledTimes(1);
    expect(document.querySelector('[data-testid="create-company-error"]')).toBeFalsy();
  });

  it("reports a genuine refusal when the lookup shows the company is still live", async () => {
    const lifecycle = vi.fn(() =>
      Promise.reject(new ApiError(0, "network_error", "network error", true)),
    );
    // The request never reached the host at all — the company is still
    // there, unarchived.
    const status = vi.fn(() =>
      Promise.resolve({ id: "acme", lifecycle: "running" } as unknown as CompanyStatus),
    );
    const provisionCompany = vi.fn((_body: ProvisionBody) =>
      Promise.resolve({ id: "acme-x" } as unknown as CompanyStatus),
    );
    await open(stubClient({ lifecycle, status, provisionCompany }));

    await fillAdminEmail();
    await submit();

    expect(status).toHaveBeenCalledWith("acme");
    // Genuinely nothing changed — reported as a real failure, no
    // provisioning attempted, no false progress.
    expect(provisionCompany).not.toHaveBeenCalled();
    expect(onCreated).not.toHaveBeenCalled();
    const error = document.querySelector('[data-testid="create-company-error"]');
    expect(error).toBeTruthy();
    expect(error!.textContent).toContain("Nothing was changed");
  });

  it("reports an unknown outcome (not a false 'Nothing was changed') when the lookup is itself ambiguous", async () => {
    const lifecycle = vi.fn(() =>
      Promise.reject(new ApiError(0, "network_error", "network error", true)),
    );
    // The reconciliation lookup drops too — genuinely can't tell either way.
    const status = vi.fn(() =>
      Promise.reject(new ApiError(0, "network_error", "network error", true)),
    );
    const provisionCompany = vi.fn((_body: ProvisionBody) =>
      Promise.resolve({ id: "acme-x" } as unknown as CompanyStatus),
    );
    await open(stubClient({ lifecycle, status, provisionCompany }));

    await fillAdminEmail();
    await submit();

    expect(provisionCompany).not.toHaveBeenCalled();
    expect(onCreated).not.toHaveBeenCalled();
    const error = document.querySelector('[data-testid="create-company-error"]');
    expect(error).toBeTruthy();
    // Must NOT assert the false "Nothing was changed" claim — the whole
    // point of this branch is that neither outcome is known.
    expect(error!.textContent).not.toContain("Nothing was changed");
    expect(error!.textContent).toContain("Couldn't confirm");
  });
});
