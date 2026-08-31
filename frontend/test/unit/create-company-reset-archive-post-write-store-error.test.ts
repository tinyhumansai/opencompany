// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { ApiError } from "@/api/types";
import type { OpenCompanyClient } from "@/api/client";
import type { CompanyStatus } from "@/api/types";
import { CreateCompanyDialog } from "@/components/create-company-dialog";

/**
 * Codex review on #1828, PR comment 3874840062: `create-company-reset-
 * archive-network-error-ambiguous.test.ts` covers a FIRST archive attempt
 * whose reply is lost (`network_error`) — the client cannot tell that apart
 * from the host having archived the company and only the reply going
 * missing. But the host's own `set_lifecycle` (`src/company/runtime.rs`)
 * persists the lifecycle change to the store, *then* appends the lifecycle
 * event; `transition()` (`src/server/provision.rs`) then reads status back
 * before answering `200`. Either of those steps can fail AFTER the archived
 * record already landed in the store — and that failure reaches this
 * client as an ordinary HTTP error response, not a dropped connection, so
 * it surfaces as a code like `store_error`, never `network_error`.
 *
 * Before the fix, only `network_error` (and a retry's `company_not_found`)
 * were reconciled against `client.status`; every other code — including
 * this genuinely-possible post-write `store_error` — fell straight into the
 * "the archive was refused, nothing was changed" branch, which may be
 * false. An operator who trusted that and closed the dialog left the
 * console showing a company that was, in fact, already archived.
 *
 * This drives the dialog through a first archive attempt that fails with
 * `store_error` (not `network_error`), covering both outcomes the
 * reconciliation lookup can land on.
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

describe("a reset whose FIRST archive attempt fails AFTER the write lands (store_error, not network_error)", () => {
  it("reconciles a lookup showing the company is gone as already archived, and proceeds", async () => {
    // The host's `set_lifecycle` persisted `archived` to the store, then
    // failed appending the lifecycle event (or `transition()` failed
    // re-reading status afterward) — an ordinary error response, not a
    // dropped connection.
    const lifecycle = vi.fn(() =>
      Promise.reject(new ApiError(500, "store_error", "store error", true)),
    );
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
    // The whole point: a `store_error` (not `network_error`) must still be
    // reconciled against `client.status` instead of trusted at face value.
    expect(status).toHaveBeenCalledWith("acme");
    expect(provisionCompany).toHaveBeenCalledTimes(1);
    expect(onCreated).toHaveBeenCalledTimes(1);
    expect(document.querySelector('[data-testid="create-company-error"]')).toBeFalsy();
  });

  it("reports a genuine refusal when the lookup shows the company is still live", async () => {
    const lifecycle = vi.fn(() =>
      Promise.reject(new ApiError(500, "store_error", "store error", true)),
    );
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
    expect(provisionCompany).not.toHaveBeenCalled();
    expect(onCreated).not.toHaveBeenCalled();
    const error = document.querySelector('[data-testid="create-company-error"]');
    expect(error).toBeTruthy();
    expect(error!.textContent).toContain("Nothing was changed");
  });
});
