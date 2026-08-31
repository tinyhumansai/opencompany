// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { OpenCompanyClient } from "@/api/client";
import type { CompanyStatus } from "@/api/types";
import { CreateCompanyDialog } from "@/components/create-company-dialog";

/**
 * Codex review on #1828 (PR comment 3861579889): the reset dialog prefills
 * its name field with the archived company's own name, and the default
 * submit sends no explicit `id`. The host derives an unset id from the name
 * (`company_id_from_name`, `runtime/builder.rs`), so the default Reset path
 * re-derived the exact id the archive just freed — and `RuntimeBuilder::build`
 * reloads any existing durable `CompanyRecord` for that id before building
 * over it (`src/runtime/builder.rs:2320`), carrying the archived lifecycle,
 * ledger and overlays forward. "Reset" would come back archived with the old
 * company's history attached, not clean.
 *
 * This is a cross-module assertion — the id collision lives in what
 * `submit()` sends `client.provisionCompany`, not in any pure helper on its
 * own — so it renders the dialog and drives a real archive→create submit,
 * the way `desk-create-name-error.test.ts` does for the same reason.
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
  // Let the archive + provision promise chain settle.
  await act(async () => {
    await Promise.resolve();
    await Promise.resolve();
    await Promise.resolve();
  });
}

// The admin email became required after codex review comment 3864885200 —
// unrelated to what this file pins (id derivation), so every submit here
// needs one filled in first or it never reaches the archive leg at all.
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

describe("resetting a company", () => {
  it("provisions the replacement under an id distinct from the archived company", async () => {
    const lifecycle = vi.fn(() => Promise.resolve());
    const provisionCompany = vi.fn((_body: ProvisionBody) =>
      Promise.resolve({ id: "whatever" } as unknown as CompanyStatus),
    );
    await open(stubClient({ lifecycle, provisionCompany }));

    await fillAdminEmail();
    await submit();

    expect(lifecycle).toHaveBeenCalledWith("archive", "acme");
    expect(provisionCompany).toHaveBeenCalledTimes(1);
    const body = provisionCompany.mock.calls[0][0];

    // The bug this guards: an unset `id` here has the host re-derive it from
    // `[company].name` in `manifest_toml`, which is still "Acme Robotics" —
    // the exact name the archived company had, and therefore the exact id
    // ("acme") that archive just freed.
    expect(body.id, "reset must send an explicit id").toBeTruthy();
    expect(body.id).not.toBe("acme");
    expect(onCreated).toHaveBeenCalledTimes(1);
  });

  it("still lets the operator name their own replacement id from Advanced", async () => {
    const provisionCompany = vi.fn((_body: ProvisionBody) =>
      Promise.resolve({ id: "whatever" } as unknown as CompanyStatus),
    );
    await open(stubClient({ provisionCompany }));

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
      setter.call(idInput, "acme-mk2");
      idInput!.dispatchEvent(new Event("input", { bubbles: true }));
    });

    await fillAdminEmail();
    await submit();

    const body = provisionCompany.mock.calls[0][0];
    expect(body.id).toBe("acme-mk2");
  });
});
