// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it } from "vitest";

import type { OpenCompanyClient } from "@/api/client";
import type { SkillUploadRow } from "@/api/skills";
import { UploadSkillDialog } from "@/views/skills/UploadSkillDialog";

/**
 * "Upload anyway" is the operator's only route past a blocking scan verdict,
 * and the dialog decides whether to offer it from what the host says about the
 * refusal.
 *
 * It used to decide by looking for the words "content scan" inside the
 * refusal sentence. That coupled a control the operator depends on to the
 * wording of a message on the other side of the API: rewording the host's
 * refusal would have removed the button, silently, with every test still
 * green. The host now states it as `scanBlocked`, and these three cases are
 * the ones that tell the two apart — the flag decides, the prose does not.
 */

let container: HTMLDivElement;
let root: Root;

function clientReturning(row: Omit<SkillUploadRow, "file">): OpenCompanyClient {
  return {
    scopeFor: () => "/api/v1/companies/acme",
    postForm: () => Promise.resolve({ results: [{ file: "probe.md", ...row }] }),
  } as unknown as OpenCompanyClient;
}

/** Renders the dialog with `open` as given, leaving its own handler inert. */
async function render(client: OpenCompanyClient, open: boolean) {
  await act(async () => {
    root.render(
      createElement(UploadSkillDialog, {
        client,
        company: "acme",
        open,
        onOpenChange: () => {},
        onUploaded: () => {},
      }),
    );
  });
}

/** Renders the dialog, drops one file in, and sends it. */
async function uploadOne(client: OpenCompanyClient) {
  await render(client, true);

  const input = document.querySelector('input[type="file"]') as HTMLInputElement;
  const file = new File(["---\nname: probe\n---\n"], "probe.md", { type: "text/markdown" });
  Object.defineProperty(input, "files", { value: [file], configurable: true });
  await act(async () => {
    input.dispatchEvent(new Event("change", { bubbles: true }));
  });

  const send = [...document.querySelectorAll("button")].find(
    (b) => b.textContent?.trim() === "Upload",
  );
  if (!send) throw new Error("the dialog's own Upload button is not on screen");
  await act(async () => {
    send.dispatchEvent(new MouseEvent("click", { bubbles: true }));
  });
}

const force = () => document.querySelector('[data-testid="skill-upload-force"]');

beforeEach(() => {
  (globalThis as unknown as { IS_REACT_ACT_ENVIRONMENT: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
});

describe("the override the dialog offers after a refusal", () => {
  it("is offered when the host says the scan blocked it, whatever the sentence says", async () => {
    await uploadOne(
      clientReturning({
        ok: false,
        error: "that skill was held back for review. Resend to store it anyway.",
        scanBlocked: true,
      }),
    );
    expect(
      force(),
      "the flag says force would store it, so the button has to be there even though the sentence never says `content scan`",
    ).not.toBeNull();
  });

  it("is withheld when the host says force would not help, even if the sentence mentions the scan", async () => {
    await uploadOne(
      clientReturning({
        ok: false,
        error: "that file has no `name` in its frontmatter, so the content scan never ran.",
        scanBlocked: false,
      }),
    );
    expect(
      force(),
      "resending cannot store a document that never validated, so offering the override would only fail again",
    ).toBeNull();
  });

  it("is withheld on a stored file", async () => {
    await uploadOne(
      clientReturning({
        ok: true,
        skill: { id: "probe", name: "Probe" } as SkillUploadRow["skill"],
        scanBlocked: false,
      }),
    );
    expect(force()).toBeNull();
  });
});

describe("what the dialog keeps when it closes", () => {
  it("drops the last run's rows when it is closed from outside its own handler", async () => {
    const client = clientReturning({
      ok: false,
      error: "that skill was refused by the content scan.",
      scanBlocked: true,
    });
    await uploadOne(client);
    expect(force(), "the run happened").not.toBeNull();

    // A parent that simply stops passing `open` — a company switch, a route
    // change — never routes the close through this dialog's `onOpenChange`.
    await render(client, false);
    await render(client, true);

    expect(
      force(),
      "reopening must not show the previous upload's verdicts as if they were this one's",
    ).toBeNull();
  });
});
