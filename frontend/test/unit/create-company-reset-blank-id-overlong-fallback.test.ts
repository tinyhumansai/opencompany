// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { OpenCompanyClient } from "@/api/client";
import type { CompanyStatus } from "@/api/types";
import { CreateCompanyDialog } from "@/components/create-company-dialog";
import { MAX_EXPLICIT_ID_LENGTH } from "@/lib/company-manifest";

/**
 * Codex review on #1828 (PR comments 3874344483 and 3874326084):
 * `create-company-reset-reject-overlong-id.test.ts` proves an OPERATOR-TYPED
 * overlong id gets refused before the archive leg runs. That check ran
 * against the raw Advanced field, though — not the id that will actually be
 * sent — and a reset's field is *pre-filled*, not blank, the moment the
 * dialog opens (`resetReplacementId(request.company)`, seeded by the
 * `useEffect` in `create-company-dialog.tsx`). So the check happened to
 * validate the right value as long as the operator left the field alone.
 *
 * It stopped being right the moment an operator actively CLEARED that
 * prefilled field back to blank: `submit` then falls back to computing
 * `resetReplacementId(request.company)` fresh — `${request.company}-<8-char
 * suffix>`, 9 characters longer than the id being replaced — but the check
 * ran against the now-empty field, which is always within bound. An existing
 * company whose id already sat within 9 characters of
 * `MAX_EXPLICIT_ID_LENGTH` produced a fallback that blew past it, discovered
 * only when `provisionCompany` failed — by which point
 * `client.lifecycle("archive", …)` had already removed the old company.
 *
 * These prove the fallback id itself is now validated before the archive
 * leg fires, even after the field was cleared back to blank.
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

async function open(client: OpenCompanyClient, company: string) {
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
  });
}

// Required after codex review comment 3864885200 — unrelated to what this
// file pins, so every submit needs one filled in first or the earlier email
// check masks the guard this file exercises.
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

describe("resetting a near-limit-id company after clearing the pre-filled Advanced field", () => {
  it("refuses before archiving, once the self-generated fallback id would exceed the bound", async () => {
    // 9 characters longer than MAX_EXPLICIT_ID_LENGTH minus the "-<suffix>"
    // resetReplacementId appends (8-char suffix + the dash) guarantees the
    // fallback overflows regardless of which suffix generator ran.
    const oldId = "c".repeat(MAX_EXPLICIT_ID_LENGTH - 4);
    const lifecycle = vi.fn(() => Promise.resolve());
    const provisionCompany = vi.fn((_body: ProvisionBody) =>
      Promise.resolve({ id: "whatever" } as unknown as CompanyStatus),
    );
    await open(stubClient({ lifecycle, provisionCompany }), oldId);

    await fillAdminEmail();
    // The field starts pre-filled with a (valid, within-bound) generated id;
    // clearing it back to blank is what re-triggers the fallback compute at
    // submit time — the exact operator action the finding is about.
    await clearAdvancedId();
    await submit();

    // The whole point: neither the destructive archive nor the (now
    // unreachable) provision call ever fires.
    expect(lifecycle).not.toHaveBeenCalled();
    expect(provisionCompany).not.toHaveBeenCalled();
    expect(onCreated).not.toHaveBeenCalled();

    const error = document.querySelector('[data-testid="create-company-error"]');
    expect(error, "no error shown").toBeTruthy();
    expect(error!.textContent).toContain("133 characters");
    expect(error!.textContent).toContain(oldId);
    // The operator's own last action was clearing the field — telling them
    // to "leave the field blank" would be actively misleading since it
    // already is.
    expect(error!.textContent).not.toContain("Leave the field blank");
  });

  it("still allows a reset whose fallback id lands within the bound", async () => {
    const oldId = "c".repeat(MAX_EXPLICIT_ID_LENGTH - 20);
    const lifecycle = vi.fn(() => Promise.resolve());
    const provisionCompany = vi.fn((_body: ProvisionBody) =>
      Promise.resolve({ id: "whatever" } as unknown as CompanyStatus),
    );
    await open(stubClient({ lifecycle, provisionCompany }), oldId);

    await fillAdminEmail();
    await clearAdvancedId();
    await submit();

    expect(lifecycle).toHaveBeenCalledWith("archive", oldId);
    expect(provisionCompany).toHaveBeenCalledTimes(1);
  });
});
