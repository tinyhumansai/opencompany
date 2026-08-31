// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { OpenCompanyClient } from "@/api/client";
import type { CompanyStatus } from "@/api/types";
import { CreateCompanyDialog } from "@/components/create-company-dialog";
import { MAX_EXPLICIT_ID_LENGTH } from "@/lib/company-manifest";

/**
 * Codex review on #1828 (PR comment 3873186322): `collidesWithArchived`
 * checks an explicit replacement id against the archived id before the
 * destructive archive leg runs, but nothing checked the id's own SHAPE.
 * `FsCompanyStore` (`store/fs.rs`, `store/paths.rs`) derives a company's
 * on-disk directory name straight from its id via `slug`, with no length
 * bound — an id past the filesystem's `NAME_MAX` component limit makes
 * `FsCompanyStore::load` fail with a non-`NotFound` I/O error, and on a
 * reset that surfaces only when `provisionCompany` runs, after the old
 * company is already archived.
 *
 * Mirrors `create-company-reset-reject-archived-id.test.ts`'s harness —
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

describe("resetting a company with an overlong replacement id typed into Advanced", () => {
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

    const overlong = "a".repeat(MAX_EXPLICIT_ID_LENGTH + 1);
    await fillAdminEmail();
    await setExplicitId(overlong);
    await submit();

    // Archiving here would leave the operator stuck: the old company gone,
    // the replacement refused by the host with a raw I/O error this client
    // never gets a chance to explain.
    expect(lifecycle).not.toHaveBeenCalled();
    expect(provisionCompany).not.toHaveBeenCalled();
    expect(onCreated).not.toHaveBeenCalled();

    const error = document.querySelector('[data-testid="create-company-error"]');
    expect(error, "no error shown").toBeTruthy();
    expect(error!.textContent).toContain("too long");
  });

  it("still allows an id at exactly the bound", async () => {
    const lifecycle = vi.fn(() => Promise.resolve());
    const provisionCompany = vi.fn((_body: ProvisionBody) =>
      Promise.resolve({ id: "whatever" } as unknown as CompanyStatus),
    );
    await open(stubClient({ lifecycle, provisionCompany }), {
      kind: "reset",
      company: "acme",
      name: "Acme Robotics",
    });

    const atBound = "a".repeat(MAX_EXPLICIT_ID_LENGTH);
    await fillAdminEmail();
    await setExplicitId(atBound);
    await submit();

    expect(lifecycle).toHaveBeenCalledWith("archive", "acme");
    expect(provisionCompany).toHaveBeenCalledTimes(1);
  });
});

describe("a plain create with an overlong id typed into Advanced", () => {
  it("refuses to provision — the same host-side failure without the archive collateral", async () => {
    const provisionCompany = vi.fn((_body: ProvisionBody) =>
      Promise.resolve({ id: "whatever" } as unknown as CompanyStatus),
    );
    await open(stubClient({ provisionCompany }), { kind: "create" });

    const overlong = "b".repeat(MAX_EXPLICIT_ID_LENGTH + 1);
    await fillAdminEmail();
    // A plain create needs a name too, or the earlier trimmedName guard
    // short-circuits before this check ever runs.
    const nameInput = document.querySelector<HTMLInputElement>("#create-company-name");
    expect(nameInput, "no name field").toBeTruthy();
    const setter = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")!.set!;
    await act(async () => {
      setter.call(nameInput, "Acme Robotics");
      nameInput!.dispatchEvent(new Event("input", { bubbles: true }));
    });
    await setExplicitId(overlong);
    await submit();

    expect(provisionCompany).not.toHaveBeenCalled();
    expect(onCreated).not.toHaveBeenCalled();
    const error = document.querySelector('[data-testid="create-company-error"]');
    expect(error, "no error shown").toBeTruthy();
    expect(error!.textContent).toContain("too long");
  });
});
