// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { OpenCompanyClient } from "@/api/client";
import type { CompanyStatus, ProvisioningInfo } from "@/api/types";
import { CreateCompanyDialog } from "@/components/create-company-dialog";

/**
 * Codex review on #1943 (PR comment 3894416362): on a wallet-mode host with a
 * slow or pending provisioning preflight, the dialog used to open with its
 * name already filled and the submit button enabled while `authMode` still
 * held its "email" default and `preflightFailed` was still false — nothing
 * distinguished "confirmed email-mode host" from "haven't heard back yet". An
 * operator who typed an email and submitted before the preflight promise
 * settled had the dialog archive the existing company and only THEN receive
 * `auth_mode_wallet_no_wallets` while trying to provision the replacement.
 *
 * This drives a real mount with a `provisioningInfo()` that never resolves
 * during the test, the same way `create-company-dialog-busy-dismiss.test.ts`
 * controls `provisionCompany`'s timing — a test that awaited the preflight
 * settling first would pass on the old code too and prove nothing about the
 * race.
 */

function stubClient(opts: { provisioningInfo: () => Promise<ProvisioningInfo> }) {
  return {
    carriesPlatformBearer: true,
    provisioningInfo: opts.provisioningInfo,
    lifecycle: vi.fn(() => Promise.resolve()),
    provisionCompany: vi.fn(() => Promise.resolve({ id: "acme-x" } as CompanyStatus)),
  } as unknown as OpenCompanyClient & {
    lifecycle: ReturnType<typeof vi.fn>;
    provisionCompany: ReturnType<typeof vi.fn>;
  };
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

describe("the create/reset dialog while the provisioning preflight is still pending", () => {
  it("refuses to submit — and never archives — before the preflight settles", async () => {
    // Never resolves during this test: the exact "slow preflight" window the
    // finding describes.
    const provisioningInfo = () => new Promise<ProvisioningInfo>(() => {});
    const client = stubClient({ provisioningInfo });
    await open(client);

    // The name field is pre-filled by the reset request, and the form still
    // shows the email field (authMode hasn't been told this is a wallet-mode
    // host yet) — exactly the state the finding describes as "opens with its
    // name already filled ... while authMode still has this email default".
    await fillAdminEmail();

    // A tick for the reset-effect and the preflight-effect to run and commit,
    // without letting the (intentionally never-resolving) preflight promise
    // settle.
    await act(async () => {
      await Promise.resolve();
      await Promise.resolve();
    });

    // The fix: submit stays disabled while the preflight is unconfirmed, so
    // an operator cannot race it the way the finding describes.
    expect(submitButton().disabled).toBe(true);

    // Clicking a disabled button dispatches no click event at all — this is
    // the actual mechanism that stops the archive leg from firing, not just
    // a cosmetic disabled attribute. On the pre-fix code this same click
    // reaches `submit()` (the button was NOT disabled there) and calls
    // `lifecycle("archive", …)` before the preflight ever settles.
    await act(async () => {
      submitButton().click();
    });

    expect((client as unknown as { lifecycle: ReturnType<typeof vi.fn> }).lifecycle).not
      .toHaveBeenCalled();
    expect((client as unknown as { provisionCompany: ReturnType<typeof vi.fn> }).provisionCompany)
      .not.toHaveBeenCalled();
    expect(onClose).not.toHaveBeenCalled();
    expect(onCreated).not.toHaveBeenCalled();
  });

  it("still resolves the mode and un-blocks submit once the preflight answers", async () => {
    let resolvePreflight!: (info: ProvisioningInfo) => void;
    const provisioningInfo = () =>
      new Promise<ProvisioningInfo>((resolve) => {
        resolvePreflight = resolve;
      });
    const client = stubClient({ provisioningInfo });
    await open(client);

    await act(async () => {
      await Promise.resolve();
      await Promise.resolve();
    });
    expect(submitButton().disabled).toBe(true);

    await act(async () => {
      resolvePreflight({ auth_mode: "wallet", wallets_required: true });
      await Promise.resolve();
      await Promise.resolve();
    });

    // Now that the host's real mode is known, the form has swapped to the
    // wallet field and submit is no longer blocked by the preflight itself
    // (still blocked on `!name.trim()`/`busy` the normal way, but not on
    // `preflightPending`).
    expect(document.querySelector("#create-company-wallet")).toBeTruthy();
    expect(document.querySelector("#create-company-admin")).toBeNull();
  });
});
