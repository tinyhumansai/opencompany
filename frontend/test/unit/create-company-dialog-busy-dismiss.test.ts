// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { OpenCompanyClient } from "@/api/client";
import type { CompanyStatus } from "@/api/types";
import { CreateCompanyDialog } from "@/components/create-company-dialog";

/**
 * Codex review on #1828 (PR comment 3861579892): the default `DialogContent`
 * close button and Escape both call `onOpenChange(false)` on the `Dialog`
 * root the same way the (`disabled={busy}`) Cancel button does, but nothing
 * gated that root callback on `busy` — so either could dismiss the dialog
 * mid-submit. `onClose` clears the parent's `request`, the dialog then
 * renders `null`, but the in-flight `submit()` keeps running: a late
 * archive-succeeded/create-failed writes its warning into a now-invisible
 * dialog, and a late success still calls `onCreated`, navigating the operator
 * into a company after they believed they had cancelled.
 *
 * This drives the Escape key exactly as an operator would, through a real
 * mount — a plain function can't observe what a root-level `onOpenChange`
 * callback does, so it earns the same jsdom render as
 * `desk-create-name-error.test.ts`.
 */

function stubClient(opts: {
  lifecycle?: () => Promise<void>;
  provisionCompany?: () => Promise<CompanyStatus>;
}) {
  return {
    carriesPlatformBearer: true,
    provisioningInfo: vi.fn(() => Promise.resolve({ auth_mode: "email", wallets_required: false })),
    lifecycle: opts.lifecycle ?? (() => Promise.resolve()),
    provisionCompany:
      opts.provisionCompany ?? (() => Promise.resolve({ id: "acme-x" } as CompanyStatus)),
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

function popup(): HTMLElement {
  const el = document.querySelector<HTMLElement>('[data-slot="dialog-content"]');
  expect(el, "dialog popup did not render").toBeTruthy();
  return el as HTMLElement;
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

// Required after codex review comment 3864885200 — unrelated to what this
// file pins (busy-dismiss gating), so the submit needs one filled in first or
// validation blocks it before the archive leg (and the busy window this test
// exercises) is ever reached.
async function fillAdminEmail(value = "ceo@acme.test") {
  const input = document.querySelector<HTMLInputElement>("#create-company-admin");
  expect(input, "no admin-email field").toBeTruthy();
  const setter = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")!.set!;
  await act(async () => {
    setter.call(input, value);
    input!.dispatchEvent(new Event("input", { bubbles: true }));
  });
}

/** Presses Escape on the popup — the same key Base UI's dialog listens for to
 * fire `onOpenChange(false)` on the root. */
async function pressEscape() {
  await act(async () => {
    popup().dispatchEvent(
      new KeyboardEvent("keydown", { key: "Escape", bubbles: true, cancelable: true }),
    );
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

describe("the create/reset dialog while a submit is in flight", () => {
  it("ignores Escape until the submit settles", async () => {
    let resolveProvision!: (status: CompanyStatus) => void;
    const provisionCompany = () =>
      new Promise<CompanyStatus>((resolve) => {
        resolveProvision = resolve;
      });
    await open(stubClient({ provisionCompany }));

    await fillAdminEmail();
    await act(async () => {
      submitButton().click();
    });
    // The archive leg has resolved and the provision leg is now pending —
    // this is the busy window the bug lets a dismissal slip through.
    await act(async () => {
      await Promise.resolve();
      await Promise.resolve();
    });
    expect(submitButton().disabled).toBe(true);

    await pressEscape();

    // Still open and still mounted: a busy dialog must not have cleared the
    // parent's request out from under the in-flight submit.
    expect(onClose).not.toHaveBeenCalled();
    expect(document.querySelector('[data-slot="dialog-content"]')).toBeTruthy();

    // Let the (now-late) success land, the way it would if the operator had
    // walked away rather than pressed Escape a second time.
    await act(async () => {
      resolveProvision({ id: "acme-x" } as CompanyStatus);
      await Promise.resolve();
      await Promise.resolve();
    });
    expect(onCreated).toHaveBeenCalledTimes(1);

    // Idle again: Escape now dismisses normally.
    await pressEscape();
    expect(onClose).toHaveBeenCalledTimes(1);
  });
});
