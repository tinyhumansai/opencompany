// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { OpenCompanyClient } from "@/api/client";

/**
 * `Settings → General` is host-backed, and stores nothing (issue #1460).
 *
 * Both cards used to be a mock over `localStorage`: the domain card hashed the
 * domain into a fake verification token and rendered five DNS records the host
 * had never heard of, and the SMTP card wrote the whole form — password
 * included — back to browser storage on every keystroke. The host had
 * implemented all of it; the console had never called it.
 *
 * So the assertions here are about provenance, not about pixels:
 *
 *   - every field on screen came off a host response, and no field came off a
 *     local derivation (`renders only the records the host returned`);
 *   - nothing the card does puts anything in `localStorage`, including the
 *     password (`writes nothing to browser storage, ever`);
 *   - a blank password field means "keep the stored one" and is omitted from
 *     the body, rather than clearing the credential with an empty string.
 *
 * `test/unit/smtp-password-never-stored.test.ts` covers the other half — that
 * `src/lib/domain.ts` has no writer left for the card to call even if it
 * wanted one.
 */

const toasts = vi.hoisted(() => ({
  base: vi.fn(),
  success: vi.fn(),
  error: vi.fn(),
  warning: vi.fn(),
  info: vi.fn(),
  message: vi.fn(),
}));

vi.mock("sonner", () => {
  const toast = Object.assign(toasts.base, {
    success: toasts.success,
    error: toasts.error,
    warning: toasts.warning,
    info: toasts.info,
    message: toasts.message,
  });
  return { toast };
});

const { DomainSettings } = await import("@/components/domain-settings");

/** Distinctive enough that a substring search over the store is meaningful. */
const SECRET = "pw-77c1e2-do-not-persist";

const DOMAIN_STATUS = {
  domain: "mail.acme.com",
  verified: false,
  records: [
    { type: "TXT" as const, name: "_opencompany.mail.acme.com", value: "oc-verify=host-issued", ttl: "3600" },
    { type: "CNAME" as const, name: "mail.acme.com", value: "in.hostmail.example", ttl: "3600" },
  ],
  checks: [
    { type: "TXT" as const, name: "_opencompany.mail.acme.com", found: true },
    { type: "CNAME" as const, name: "mail.acme.com", found: false },
  ],
};

const SMTP_STATUS = {
  configured: true,
  host: "smtp.postmarkapp.com",
  port: 2525,
  security: "ssl" as const,
  username: "apikey",
  from_name: "Acme",
  from_email: "hello@mail.acme.com",
};

interface Calls {
  put: { path: string; body: unknown }[];
  post: { path: string; body: unknown }[];
}

/**
 * A client answering the two reads, and recording the writes.
 *
 * Routed on the path rather than on call order: the two cards mount
 * independently and nothing guarantees which read resolves first.
 */
function fakeClient(overrides: { domain?: unknown; smtp?: unknown } = {}) {
  const calls: Calls = { put: [], post: [] };
  const client = {
    scopeFor: () => "/api/v1/companies/acme",
    get: (path: string) => {
      const answer = path.endsWith("/domain")
        ? (overrides.domain ?? DOMAIN_STATUS)
        : (overrides.smtp ?? SMTP_STATUS);
      return answer instanceof Error ? Promise.reject(answer) : Promise.resolve(answer);
    },
    put: (path: string, body: unknown) => {
      calls.put.push({ path, body });
      return Promise.resolve(path.endsWith("/domain") ? DOMAIN_STATUS : SMTP_STATUS);
    },
    post: (path: string, body: unknown) => {
      calls.post.push({ path, body });
      return Promise.resolve(
        path.endsWith("/domain/verify") ? DOMAIN_STATUS : { ok: true, message: "Sent." },
      );
    },
  } as unknown as OpenCompanyClient;
  return { client, calls };
}

let container: HTMLDivElement;
let root: Root;

async function show(client: OpenCompanyClient) {
  await act(async () => {
    root.render(createElement(DomainSettings, { client, company: "acme", canManage: true }));
  });
}

function at(testid: string): HTMLElement | null {
  return container.querySelector<HTMLElement>(`[data-testid="${testid}"]`);
}

function allAt(testid: string): HTMLElement[] {
  return Array.from(container.querySelectorAll<HTMLElement>(`[data-testid="${testid}"]`));
}

/** Types into a controlled input the way React wants to be told about it. */
async function type(testid: string, value: string) {
  const input = at(testid) as HTMLInputElement;
  const setter = Object.getOwnPropertyDescriptor(
    window.HTMLInputElement.prototype,
    "value",
  )?.set;
  await act(async () => {
    setter?.call(input, value);
    input.dispatchEvent(new Event("input", { bubbles: true }));
  });
}

async function click(testid: string) {
  await act(async () => {
    at(testid)?.dispatchEvent(new MouseEvent("click", { bubbles: true }));
  });
}

/** Every value currently in `localStorage`, concatenated. */
function entireStore(): string {
  const parts: string[] = [];
  for (let i = 0; i < localStorage.length; i++) {
    const key = localStorage.key(i);
    if (key === null) continue;
    parts.push(key, localStorage.getItem(key) ?? "");
  }
  return parts.join("\n");
}

beforeEach(() => {
  (globalThis as unknown as { IS_REACT_ACT_ENVIRONMENT: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
  localStorage.clear();
  vi.clearAllMocks();
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
  vi.restoreAllMocks();
});

describe("what the cards render", () => {
  it("fills both forms from the host response, not from a local default", async () => {
    // Port 2525, SSL and the Postmark host are all values the old card could
    // not have produced: its defaults were 587 and STARTTLS with empty strings
    // everywhere else. Reading them back is the evidence that the host answer
    // is what reached the DOM.
    const { client } = fakeClient();
    await show(client);

    expect((at("smtp-host") as HTMLInputElement).value).toBe("smtp.postmarkapp.com");
    expect((at("smtp-port") as HTMLInputElement).value).toBe("2525");
    expect((at("smtp-username") as HTMLInputElement).value).toBe("apikey");
    expect((at("smtp-from-name") as HTMLInputElement).value).toBe("Acme");
    expect((at("smtp-from-email") as HTMLInputElement).value).toBe("hello@mail.acme.com");
    expect(container.textContent).toContain("mail.acme.com");
  });

  it("renders only the records the host returned", async () => {
    // The old card always rendered five, derived client-side from the domain
    // string. Two rows, both carrying host-issued values, is the assertion that
    // no local generator survived — including as a fallback.
    const { client } = fakeClient();
    await show(client);

    expect(allAt("dns-record-row")).toHaveLength(2);
    expect(container.textContent).toContain("oc-verify=host-issued");
    expect(container.textContent).toContain("in.hostmail.example");
    expect(container.textContent).not.toContain("opencompany.host");
  });

  it("distinguishes 'not checked yet' from 'checked and not found'", async () => {
    // The whole reason the host returns `checks` rather than just `verified`:
    // the first is on the operator to press the button, the second is on their
    // registrar. A single "Pending" would say neither.
    const { client } = fakeClient();
    await show(client);
    expect(at("domain-check-summary")?.textContent).toBe("1 of 2 records found.");

    act(() => root.unmount());
    root = createRoot(container);
    const unchecked = fakeClient({ domain: { ...DOMAIN_STATUS, checks: undefined } });
    await show(unchecked.client);
    expect(at("domain-check-summary")?.textContent).toBe("Not checked yet.");
  });

  it("shows the add form, and no fabricated records, when no domain is set", async () => {
    const { client } = fakeClient({ domain: { domain: "", verified: false, records: [] } });
    await show(client);

    expect(at("domain-input")).not.toBeNull();
    expect(allAt("dns-record-row")).toHaveLength(0);
  });

  it("reports a failed read instead of falling back to a remembered draft", async () => {
    // A read that fails must not be papered over with a stale local copy —
    // that is precisely the disagreement the store was retired to prevent.
    const { client } = fakeClient({ domain: new Error("host unreachable") });
    await show(client);

    expect(at("domain-load-error")?.textContent).toContain("host unreachable");
    expect(at("domain-input")).toBeNull();
    // The SMTP card reads a different route and must survive its neighbour.
    expect((at("smtp-host") as HTMLInputElement).value).toBe("smtp.postmarkapp.com");
  });
});

describe("saving the SMTP card", () => {
  it("writes nothing to browser storage, ever", async () => {
    // The regression, stated as the property rather than as a key lookup: type
    // the credential, save it, and assert the whole store is still empty.
    const { client } = fakeClient();
    await show(client);

    await type("smtp-password", SECRET);
    expect(entireStore()).not.toContain(SECRET);

    await click("smtp-save");

    expect(entireStore()).not.toContain(SECRET);
    expect(entireStore()).not.toContain("oc-mail");
    expect(localStorage.length).toBe(0);
  });

  it("sends the typed password once and then clears the field", async () => {
    // Cleared for the same reason `HostingView` clears its API key: a
    // credential left sitting in a form field is one screen-share from a leak.
    const { client, calls } = fakeClient();
    await show(client);

    await type("smtp-password", SECRET);
    await click("smtp-save");

    expect(calls.put).toHaveLength(1);
    expect(calls.put[0].body).toMatchObject({ password: SECRET, port: 2525, security: "ssl" });
    expect((at("smtp-password") as HTMLInputElement).value).toBe("");
  });

  it("omits the password when the field is blank, rather than clearing it", async () => {
    // A blank field means "leave the stored one alone". Sending `""` would
    // wipe a credential the operator cannot see and did not touch.
    const { client, calls } = fakeClient();
    await show(client);

    await type("smtp-port", "465");
    await click("smtp-save");

    expect(calls.put).toHaveLength(1);
    const body = calls.put[0].body as Record<string, unknown>;
    expect("password" in body).toBe(false);
    expect(body.port).toBe(465);
  });

  it("will not offer a test send until the host has something stored to test", async () => {
    // The send goes through what the HOST holds. Gating on the form would light
    // the button up on a typed-but-unsaved password and then report a verdict
    // about a different configuration — the same class of lie the card told
    // before #1460, one button along.
    const { client } = fakeClient({ smtp: { configured: false, host: "smtp.postmarkapp.com" } });
    await show(client);

    expect((at("smtp-test") as HTMLButtonElement).disabled).toBe(true);
    expect(at("smtp-test-hint")?.textContent).toContain("Save a complete configuration");
  });

  it("offers the test send once the host reports a stored configuration", async () => {
    // The control: the assertion above is only worth having if the button is
    // reachable at all.
    const { client, calls } = fakeClient();
    await show(client);

    expect((at("smtp-test") as HTMLButtonElement).disabled).toBe(false);
    expect(at("smtp-test-hint")).toBeNull();

    await click("smtp-test");
    expect(calls.post).toHaveLength(1);
    expect(calls.post[0].path).toContain("/smtp/test");
    // The host's own sentence, verbatim.
    expect(toasts.success).toHaveBeenCalledWith("Sent.");
  });

  it("refuses a port the host would reject with a serde error", async () => {
    // The host takes a `u16`. Caught here so the operator gets a sentence they
    // can act on instead of a deserialization message.
    const { client, calls } = fakeClient();
    await show(client);

    await type("smtp-port", "99999");
    await click("smtp-save");

    expect(calls.put).toHaveLength(0);
    expect(toasts.error).toHaveBeenCalledWith("Port must be a whole number between 1 and 65535.");
  });
});

describe("the domain card's writes", () => {
  it("refuses a bare label before spending a round trip on it", async () => {
    const { client, calls } = fakeClient({ domain: { domain: "", verified: false, records: [] } });
    await show(client);

    await type("domain-input", "acme");
    await click("domain-add");

    expect(calls.put).toHaveLength(0);
    expect(toasts.error).toHaveBeenCalledWith("Enter a valid domain, e.g. mail.acme.com");
  });

  it("sends the domain lowercased and trimmed", async () => {
    const { client, calls } = fakeClient({ domain: { domain: "", verified: false, records: [] } });
    await show(client);

    await type("domain-input", "  Mail.ACME.com  ");
    await click("domain-add");

    expect(calls.put).toHaveLength(1);
    expect(calls.put[0].body).toEqual({ domain: "mail.acme.com" });
    expect(entireStore()).not.toContain("oc-mail");
  });

  it("removes the domain with the host's empty-string sentinel", async () => {
    const { client, calls } = fakeClient();
    await show(client);

    await click("domain-remove");

    expect(calls.put).toHaveLength(1);
    expect(calls.put[0].body).toEqual({ domain: "" });
  });
});
