// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { OpenCompanyClient } from "@/api/client";
import type { CompanyStatus } from "@/api/types";
import { CreateCompanyDialog } from "@/components/create-company-dialog";

/**
 * Codex review on #1828 (PR comment 3862711345): a reset's admin-email field
 * is not inside a submitted `<form>` — the button calls `submit()` directly
 * — so the browser's own `type="email"` validation never blocks it. Before
 * this fix, a malformed address reached the host only after the archive leg
 * had already run: `manifest.validate()` rejects `[users].admins` on the
 * *second* half of the request, leaving the operator staring at "Archived X,
 * but couldn't create the new company" for what was, from their side, a
 * typo. `adminEmailProblem` (already used by `company-setup.ts` for the
 * identical class of bug) now runs before the archive leg at all.
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

async function setAdminEmail(value: string) {
  const input = document.querySelector<HTMLInputElement>("#create-company-admin");
  expect(input, "no admin-email field").toBeTruthy();
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

describe("resetting a company with a malformed admin email", () => {
  it("refuses to archive, catching the typo before the destructive leg runs", async () => {
    const lifecycle = vi.fn(() => Promise.resolve());
    const provisionCompany = vi.fn((_body: ProvisionBody) =>
      Promise.resolve({ id: "whatever" } as unknown as CompanyStatus),
    );
    await open(stubClient({ lifecycle, provisionCompany }));

    await setAdminEmail("not-an-email");
    await submit();

    // The bug this guards: the old code archived "acme" here, then failed
    // provisioning on the host's manifest validator, leaving the operator
    // with an archived company and an honest-but-avoidable half-state error.
    expect(lifecycle).not.toHaveBeenCalled();
    expect(provisionCompany).not.toHaveBeenCalled();
    expect(onCreated).not.toHaveBeenCalled();

    const error = document.querySelector('[data-testid="create-company-error"]');
    expect(error, "no error shown").toBeTruthy();
    expect(error!.textContent).toContain("email");
  });

  it("still proceeds normally with a well-formed email", async () => {
    const lifecycle = vi.fn(() => Promise.resolve());
    const provisionCompany = vi.fn((_body: ProvisionBody) =>
      Promise.resolve({ id: "acme-x" } as unknown as CompanyStatus),
    );
    await open(stubClient({ lifecycle, provisionCompany }));

    await setAdminEmail("ceo@acme.test");
    await submit();

    expect(lifecycle).toHaveBeenCalledWith("archive", "acme");
    expect(provisionCompany).toHaveBeenCalledTimes(1);
    expect(onCreated).toHaveBeenCalledTimes(1);
  });

  it("refuses a blank admin email — this dialog cannot confirm a bootstrap admin exists (codex review on #1828, PR comment 3864885200)", async () => {
    const lifecycle = vi.fn(() => Promise.resolve());
    const provisionCompany = vi.fn((_body: ProvisionBody) =>
      Promise.resolve({ id: "acme-x" } as unknown as CompanyStatus),
    );
    await open(stubClient({ lifecycle, provisionCompany }));

    await submit();

    // Blank used to be a fine answer: the field was optional on the theory
    // that "whoever provisioned this host can already sign in". That is only
    // true when the deployment injected `OPENCOMPANY_ADMIN_EMAIL` — a
    // self-hosted `serve` with no manager leaves it unset (a documented
    // no-op, AGENTS.md), and this dialog has no way to tell the two apart.
    // Archiving the old company before creating a replacement nobody can
    // reach is the destructive half of the bug; refusing here keeps the old
    // company intact instead.
    expect(lifecycle).not.toHaveBeenCalled();
    expect(provisionCompany).not.toHaveBeenCalled();
    expect(onCreated).not.toHaveBeenCalled();

    const error = document.querySelector('[data-testid="create-company-error"]');
    expect(error, "no error shown").toBeTruthy();
    expect(error!.textContent).toContain("address");
  });
});
