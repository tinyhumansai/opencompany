// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { ApiError } from "@/api/types";
import type { OpenCompanyClient } from "@/api/client";
import type { CompanyStatus } from "@/api/types";
import { CreateCompanyDialog } from "@/components/create-company-dialog";

/**
 * Codex review on #1828 (PR comment 3861770485): if the archive request
 * reaches the host and completes, but the connection drops before the reply
 * is read, `ApiClient.request` throws the same `network_error` it would for
 * a request that never landed at all — there is no way to distinguish
 * "refused" from "already done" from the caught exception. The dialog's old
 * behavior reported that as a fresh failure ("Nothing was changed") and left
 * `archived` false, so a retry re-sends the archive call. The host correctly
 * answers `company_not_found` for a second archive of an id it already
 * removed — but the old code treated *that* as a fresh failure too, trapping
 * the operator in a retry loop that can never reach the create leg.
 *
 * Simulates the retry directly: `lifecycle` rejects with `company_not_found`
 * on the (first, in this test) call, standing in for "a retry after a lost
 * reply", and asserts the dialog proceeds to provision instead of reporting
 * a failure.
 */

type ProvisionBody = { manifest_toml: string; id?: string };

function stubClient(opts: {
  lifecycle: ReturnType<typeof vi.fn>;
  provisionCompany?: ReturnType<typeof vi.fn<(body: ProvisionBody) => Promise<CompanyStatus>>>;
}) {
  return {
    carriesPlatformBearer: true,
    provisioningInfo: vi.fn(() => Promise.resolve({ auth_mode: "email", wallets_required: false })),
    lifecycle: opts.lifecycle,
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
  });
}

// Required after codex review comment 3864885200 — unrelated to what this
// file pins (archive-not-found reconciliation), so every submit needs one
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

describe("a reset whose archive answers company_not_found", () => {
  it("treats it as already archived and proceeds to provision the replacement", async () => {
    const lifecycle = vi.fn(() =>
      Promise.reject(new ApiError(404, "company_not_found", "company not found: acme", true)),
    );
    const provisionCompany = vi.fn((_body: ProvisionBody) =>
      Promise.resolve({ id: "acme-x" } as unknown as CompanyStatus),
    );
    await open(stubClient({ lifecycle, provisionCompany }));

    await fillAdminEmail();
    await submit();

    expect(lifecycle).toHaveBeenCalledWith("archive", "acme");
    // The dangerous half-state bug: an operator stuck seeing "Nothing was
    // changed" and unable to ever provision the replacement.
    expect(provisionCompany).toHaveBeenCalledTimes(1);
    expect(onCreated).toHaveBeenCalledTimes(1);
    expect(document.querySelector('[data-testid="create-company-error"]')).toBeFalsy();
  });

  it("still reports a real archive refusal (not company_not_found) as a failure", async () => {
    const lifecycle = vi.fn(() =>
      Promise.reject(new ApiError(500, "internal_error", "internal error", true)),
    );
    const provisionCompany = vi.fn((_body: ProvisionBody) =>
      Promise.resolve({ id: "acme-x" } as unknown as CompanyStatus),
    );
    await open(stubClient({ lifecycle, provisionCompany }));

    await fillAdminEmail();
    await submit();

    expect(lifecycle).toHaveBeenCalledWith("archive", "acme");
    expect(provisionCompany).not.toHaveBeenCalled();
    expect(onCreated).not.toHaveBeenCalled();
    expect(document.querySelector('[data-testid="create-company-error"]')).toBeTruthy();
  });
});
