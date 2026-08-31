// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { OpenCompanyClient } from "@/api/client";
import type { CompanyStatus } from "@/api/types";
import { CreateCompanyDialog } from "@/components/create-company-dialog";

/**
 * Codex review on #1828 (PR comment 3875745309): `explicitIdProblem`'s
 * slug-stability check (`slug(candidateId) === candidateId`,
 * `create-company-reject-slug-unsafe-id.test.ts`) accepts a trailing period
 * — `slug` (`store/paths.rs`) allowlists `.` and passes it through
 * unmodified at any position, so `slug("acme.") === "acme."` and the id
 * sails through. But Windows Win32 path handling strips a trailing period
 * from a path component before the directory is ever created — already
 * documented and defended against for secret filenames
 * (`percent_encode`, `store/paths.rs`) but not for company ids. On a
 * Windows-backed desktop or self-hosted host, `Bundle::new` names the
 * directory `acme.`, the OS creates it as `acme`, and `FsCompanyStore::list`
 * reconstructs the id from that directory name on the next read — so the
 * bundle is created under `acme.` and comes back after a restart as `acme`,
 * the same silent-identity-change failure the slug-stability check exists
 * to prevent, just reached through OS normalization instead of `slug`'s own
 * folding. On a reset, this is discovered only after the old company is
 * already archived.
 *
 * Mirrors `create-company-reset-reject-dot-segment-id.test.ts`'s harness —
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

describe("resetting a company with 'acme.' typed into Advanced as the replacement id", () => {
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
    await setExplicitId("acme.");
    await submit();

    // Archiving here would retire the old company for a replacement whose
    // id silently changes out from under it on a Windows host's very next
    // restart — the OS strips the trailing period from the directory name
    // before `list` ever reads it back.
    expect(lifecycle).not.toHaveBeenCalled();
    expect(provisionCompany).not.toHaveBeenCalled();
    expect(onCreated).not.toHaveBeenCalled();

    const error = document.querySelector('[data-testid="create-company-error"]');
    expect(error, "no error shown").toBeTruthy();
    expect(error!.textContent).toContain("end with a period");
  });
});

describe("a plain create with 'acme.' typed into Advanced as the id", () => {
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
    await setExplicitId("acme.");
    await submit();

    expect(provisionCompany).not.toHaveBeenCalled();
    expect(onCreated).not.toHaveBeenCalled();
    const error = document.querySelector('[data-testid="create-company-error"]');
    expect(error, "no error shown").toBeTruthy();
    expect(error!.textContent).toContain("end with a period");
  });
});
