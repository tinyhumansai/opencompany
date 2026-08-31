// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { ApiError } from "@/api/types";
import type { OpenCompanyClient } from "@/api/client";
import type { CompanyStatus } from "@/api/types";
import { CreateCompanyDialog } from "@/components/create-company-dialog";

/**
 * Codex review on #1828, PR comment 3874947935: the sibling
 * `create-company-reset-archive-post-write-store-error.test.ts` pins the
 * reconciliation lookup for the two outcomes it distinguished by *whether
 * the lookup itself succeeded* — `company_not_found` (gone) vs. a resolved
 * status (treated as "still live, nothing changed"). But a resolved lookup
 * is not always "still live": `set_lifecycle` (`src/company/runtime.rs`)
 * persists `archived` to the store BEFORE appending the lifecycle event,
 * and `archive` (`src/server/provision.rs`) only removes the runtime from
 * the registry when `transition()` answers `200` — so a post-write failure
 * there leaves the runtime registered with `archived` already persisted.
 * The reconciliation `client.status()` call then succeeds (the id is still
 * addressable) and returns `lifecycle: "archived"` — a resolved lookup that
 * is NOT "still live". The pre-fix code discarded the returned status
 * entirely and treated any successful lookup as proof the archive "did not
 * take", asserting "Nothing was changed" for a company that in fact was
 * already gone.
 *
 * This drives that exact case: the reconciliation lookup resolves (not
 * `company_not_found`) with `lifecycle: "archived"`, and must be reconciled
 * the same way a `company_not_found` reply is — proceed to the create leg —
 * not reported as a refusal.
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

describe("a reset whose FIRST archive attempt fails AFTER the write lands, and the reconciliation lookup itself resolves", () => {
  it("treats a resolved lookup reporting lifecycle:\"archived\" as the archive having landed, and proceeds", async () => {
    const lifecycle = vi.fn(() =>
      Promise.reject(new ApiError(500, "store_error", "store error", true)),
    );
    // The lookup does NOT throw `company_not_found` — the runtime is still
    // registered (the archive response never reached 200, so the registry
    // removal never ran) — but the persisted record already shows
    // `archived`, because `set_lifecycle` saves the record before it
    // appends the lifecycle event that failed.
    const status = vi.fn(() =>
      Promise.resolve({ id: "acme", lifecycle: "archived" } as unknown as CompanyStatus),
    );
    const provisionCompany = vi.fn((_body: ProvisionBody) =>
      Promise.resolve({ id: "acme-x" } as unknown as CompanyStatus),
    );
    await open(stubClient({ lifecycle, status, provisionCompany }));

    await fillAdminEmail();
    await submit();

    expect(lifecycle).toHaveBeenCalledWith("archive", "acme");
    expect(status).toHaveBeenCalledWith("acme");
    // The whole point: a resolved lookup showing `lifecycle: "archived"`
    // must be reconciled as the archive having taken, not reported as
    // "Nothing was changed".
    expect(provisionCompany).toHaveBeenCalledTimes(1);
    expect(onCreated).toHaveBeenCalledTimes(1);
    expect(document.querySelector('[data-testid="create-company-error"]')).toBeFalsy();
  });
});
