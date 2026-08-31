// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { OpenCompanyClient } from "@/api/client";
import type { CompanyStatus } from "@/api/types";
import { CreateCompanyDialog } from "@/components/create-company-dialog";

/**
 * Codex review on #1828 (PR comment 3875297936): `explicitIdProblem` refused
 * the two reserved dot-segments but accepted anything else, including ids
 * containing characters `slug` (`store/paths.rs`) does not pass through.
 * `Bundle::new` derives the on-disk directory from `slug(id)` — every
 * character outside `[A-Za-z0-9._-]` folds to `_` — but `FsCompanyStore::list`
 * reconstructs a company's id FROM that directory name on every subsequent
 * read (`entry.file_name()`), never from anything stored inside the bundle.
 * So an id like `acme corp` or `acme/ops` is accepted by provisioning and
 * echoed back correctly in that same response, but any read that goes
 * through `list` — in particular a restart — reconstructs it as the slugged
 * form (`acme_corp`), a silent identity change the connection profile this
 * client already saved under the original id has no way to follow, and the
 * next request for it gets `company_not_found`.
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

describe.each(["acme corp" as const, "acme/ops" as const])(
  "resetting a company with %j typed into Advanced as the replacement id",
  (unsafeId) => {
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
      await setExplicitId(unsafeId);
      await submit();

      // Archiving here would retire the old company for a replacement whose
      // id silently changes out from under it on the very next restart —
      // `acme corp`/`acme/ops` reconstruct as `acme_corp`/`acme_ops` from
      // the slugged directory name, so the connection profile saved under
      // the typed id would get `company_not_found` forever after.
      expect(lifecycle).not.toHaveBeenCalled();
      expect(provisionCompany).not.toHaveBeenCalled();
      expect(onCreated).not.toHaveBeenCalled();

      const error = document.querySelector('[data-testid="create-company-error"]');
      expect(error, "no error shown").toBeTruthy();
      expect(error!.textContent).toContain("letters, numbers");
    });
  },
);

describe.each(["acme corp" as const, "acme/ops" as const])(
  "a plain create with %j typed into Advanced as the id",
  (unsafeId) => {
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
      await setExplicitId(unsafeId);
      await submit();

      expect(provisionCompany).not.toHaveBeenCalled();
      expect(onCreated).not.toHaveBeenCalled();
      const error = document.querySelector('[data-testid="create-company-error"]');
      expect(error, "no error shown").toBeTruthy();
      expect(error!.textContent).toContain("letters, numbers");
    });
  },
);

/**
 * `resetReplacementId` derives the fallback id from the OLD company's own id
 * (`${oldId}-${randomIdSuffix()}`), not from a name — so a legacy company
 * whose id predates this charset check (created before it existed, or
 * provisioned outside this client entirely) can still hand the dialog a
 * self-generated fallback that fails it. Before this fix, that landed on the
 * dialog's `selfGenerated` branch, which unconditionally showed a "couldn't
 * generate a short enough id" message — accurate for the length check this
 * branch was written for, wrong for a charset problem it did not anticipate.
 */
describe("resetting a company whose OWN id is already slug-unsafe (legacy)", () => {
  it("shows the actual charset problem, not a hardcoded length message", async () => {
    const lifecycle = vi.fn(() => Promise.resolve());
    const provisionCompany = vi.fn((_body: ProvisionBody) =>
      Promise.resolve({ id: "whatever" } as unknown as CompanyStatus),
    );
    await open(stubClient({ lifecycle, provisionCompany }), {
      kind: "reset",
      company: "acme corp",
      name: "Acme Robotics",
    });

    await fillAdminEmail();
    // Advanced is never opened — the id field is left on its
    // `resetReplacementId("acme corp")` default, so this is the
    // self-generated path, not an operator-typed one.
    await submit();

    expect(lifecycle).not.toHaveBeenCalled();
    expect(provisionCompany).not.toHaveBeenCalled();
    expect(onCreated).not.toHaveBeenCalled();

    const error = document.querySelector('[data-testid="create-company-error"]');
    expect(error, "no error shown").toBeTruthy();
    expect(error!.textContent).toContain("letters, numbers");
    expect(error!.textContent).not.toContain("short enough");
  });
});
