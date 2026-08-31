// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { OpenCompanyClient } from "@/api/client";
import type { CompanyStatus } from "@/api/types";
import { CreateCompanyDialog } from "@/components/create-company-dialog";

/**
 * Codex review on #1828 (PR comment 3861770475): `resetReplacementId` seeds a
 * fresh default id, but the Advanced field stays editable, and nothing
 * stopped an operator from changing it back to the archived company's own id
 * — a likely move when trying to preserve the slug. Submitting that value
 * archives the old company and then reprovisions under the exact id that was
 * just freed, recreating the collision #1807's first fix (3d74f98d9) exists
 * to prevent: `RuntimeBuilder::build` reloads any existing durable record for
 * an id before building over it, so the "clean" replacement comes back
 * carrying the archived lifecycle, ledger and overlays.
 *
 * Renders the dialog and edits the real Advanced input, the way
 * `create-company-reset-fresh-id.test.ts` does for the same reason.
 */

type ProvisionBody = { manifest_toml: string; id?: string };

function stubClient(opts: {
  lifecycle?: ReturnType<typeof vi.fn>;
  provisionCompany?: ReturnType<typeof vi.fn<(body: ProvisionBody) => Promise<CompanyStatus>>>;
}) {
  return {
    carriesPlatformBearer: true,
    provisioningInfo: vi.fn(() => Promise.resolve({ auth_mode: "email", wallets_required: false })),
    lifecycle: opts.lifecycle ?? vi.fn(() => Promise.resolve()),
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

async function open(client: OpenCompanyClient, company = "acme") {
  await act(async () => {
    root.render(
      createElement(CreateCompanyDialog, {
        client,
        request: { kind: "reset", company, name: "Acme Robotics" },
        onClose,
        onCreated,
      }),
    );
  });
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
// file pins (the archived-id collision guard), so every submit needs one
// filled in first or the earlier email check masks the guard this file
// exercises.
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

describe("resetting a company with the replacement id edited back to the archived id", () => {
  it("refuses to archive or provision, and tells the operator why", async () => {
    const lifecycle = vi.fn(() => Promise.resolve());
    const provisionCompany = vi.fn((_body: ProvisionBody) =>
      Promise.resolve({ id: "whatever" } as unknown as CompanyStatus),
    );
    await open(stubClient({ lifecycle, provisionCompany }));

    // The archived company's own id, typed back in from Advanced.
    await fillAdminEmail();
    await setExplicitId("acme");
    await submit();

    // Neither half of the reset should have run — archiving "acme" here
    // would leave the operator stuck with no clean replacement to retry into.
    expect(lifecycle).not.toHaveBeenCalled();
    expect(provisionCompany).not.toHaveBeenCalled();
    expect(onCreated).not.toHaveBeenCalled();

    const error = document.querySelector('[data-testid="create-company-error"]');
    expect(error, "no error shown").toBeTruthy();
    expect(error!.textContent).toContain("acme");
  });
});

/**
 * Codex review on #1828 (PR comment 3862711330): the same guard, but under
 * shared-single-DB tenant namespacing, where `request.company` (the
 * archived company's `CompanyStatus.id`) is the namespaced form
 * (`tenant-a--acme`) and an operator types the *bare* id back in — a
 * plausible move, since that bare form is what they likely named the
 * company when it was first provisioned, before any tenant prefix was
 * attached.
 */
describe("resetting a tenant-namespaced company with the bare archived id typed back in", () => {
  it("refuses to archive or provision, even though the strings don't match exactly", async () => {
    const lifecycle = vi.fn(() => Promise.resolve());
    const provisionCompany = vi.fn((_body: ProvisionBody) =>
      Promise.resolve({ id: "whatever" } as unknown as CompanyStatus),
    );
    await open(stubClient({ lifecycle, provisionCompany }), "tenant-a--acme");

    // The bare id, not the full namespaced one the archived company actually
    // carries — this is exactly the form an exact-string check misses.
    await fillAdminEmail();
    await setExplicitId("acme");
    await submit();

    expect(lifecycle).not.toHaveBeenCalled();
    expect(provisionCompany).not.toHaveBeenCalled();
    expect(onCreated).not.toHaveBeenCalled();
    expect(document.querySelector('[data-testid="create-company-error"]')).toBeTruthy();
  });
});
