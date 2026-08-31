// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { OpenCompanyClient } from "@/api/client";
import type { CompanyStatus } from "@/api/types";
import { CreateCompanyDialog } from "@/components/create-company-dialog";

/**
 * Codex review on #1828 (PR comment 3874738990): `explicitIdProblem` only
 * checked an operator-typed id's length, not its shape. `slug`
 * (`store/paths.rs`) passes `.` straight through as one of its three
 * filesystem-safe characters, so `Bundle::new` joins a `.` or `..` id onto
 * `home/companies` unmodified — `..` resolves the bundle directory to `home`
 * itself, landing the manifest save outside any per-company directory
 * instead of failing closed. `OpenCompanyClient.scope()` compounds this on
 * the request path: `encodeURIComponent` leaves `.` unescaped, so ordinary
 * URL normalization collapses a `/companies/..` route before it reaches the
 * host, making the new company unreachable through its expected
 * `/companies/{id}` route.
 *
 * Mirrors `create-company-reset-reject-overlong-id.test.ts`'s harness —
 * same Advanced-field editing, same "neither half of the reset ran" shape
 * of assertion.
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
  ).find(
    (b) =>
      b.textContent?.trim().startsWith("Archive & start clean") ||
      b.textContent?.trim() === "Create company",
  );
  expect(match, "no submit button found").toBeTruthy();
  return match as HTMLButtonElement;
}

async function open(
  client: OpenCompanyClient,
  request: { kind: "reset"; company: string; name: string } | { kind: "create" },
) {
  await act(async () => {
    root.render(createElement(CreateCompanyDialog, { client, request, onClose, onCreated }));
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

describe.each(["." as const, ".." as const])(
  "resetting a company with %j typed into Advanced as the replacement id",
  (dotSegment) => {
    it("refuses to archive or provision, before either leg runs", async () => {
      const lifecycle = vi.fn(() => Promise.resolve());
      const provisionCompany = vi.fn((_body: ProvisionBody) =>
        Promise.resolve({ id: "whatever" } as unknown as CompanyStatus),
      );
      await open(stubClient({ lifecycle, provisionCompany }), {
        kind: "reset",
        company: "acme",
        name: "Acme Robotics",
      });

      await fillAdminEmail();
      await setExplicitId(dotSegment);
      await submit();

      // Archiving here would land the replacement's manifest outside any
      // per-company directory (`..` resolves the bundle dir to `home`
      // itself) instead of failing closed, with the old company already
      // gone.
      expect(lifecycle).not.toHaveBeenCalled();
      expect(provisionCompany).not.toHaveBeenCalled();
      expect(onCreated).not.toHaveBeenCalled();

      const error = document.querySelector('[data-testid="create-company-error"]');
      expect(error, "no error shown").toBeTruthy();
      expect(error!.textContent).toContain("reserved path segment");
    });
  },
);

describe.each(["." as const, ".." as const])(
  "a plain create with %j typed into Advanced as the id",
  (dotSegment) => {
    it("refuses to provision — the same host-side hazard without the archive collateral", async () => {
      const provisionCompany = vi.fn((_body: ProvisionBody) =>
        Promise.resolve({ id: "whatever" } as unknown as CompanyStatus),
      );
      await open(stubClient({ provisionCompany }), { kind: "create" });

      await fillAdminEmail();
      const nameInput = document.querySelector<HTMLInputElement>("#create-company-name");
      expect(nameInput, "no name field").toBeTruthy();
      const setter = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")!.set!;
      await act(async () => {
        setter.call(nameInput, "Acme Robotics");
        nameInput!.dispatchEvent(new Event("input", { bubbles: true }));
      });
      await setExplicitId(dotSegment);
      await submit();

      expect(provisionCompany).not.toHaveBeenCalled();
      expect(onCreated).not.toHaveBeenCalled();
      const error = document.querySelector('[data-testid="create-company-error"]');
      expect(error, "no error shown").toBeTruthy();
      expect(error!.textContent).toContain("reserved path segment");
    });
  },
);
