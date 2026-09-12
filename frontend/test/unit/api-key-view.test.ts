// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it } from "vitest";

import type { OpenCompanyClient } from "@/api/client";
import type { CompanyCredentialStatus } from "@/api/credential";
import { captureKeyLink } from "@/lib/pending-key-link";
import { ApiKeyView } from "@/views/connections/ApiKeyView";

let container: HTMLDivElement;
let root: Root;

function credential(overrides: Partial<CompanyCredentialStatus> = {}): CompanyCredentialStatus {
  return {
    configured: true,
    source: "company",
    notice: "notice",
    hubLink: false,
    ...overrides,
  };
}

/** A client whose `/auth/me` always answers as a non-admin, so the credential
 * card's own network calls stay quiet and out of scope for these tests. */
function clientFor(handlers: {
  credential: () => Promise<CompanyCredentialStatus>;
  billing: () => Promise<unknown>;
}): OpenCompanyClient {
  return {
    scopeFor: () => "/api/v1/companies/acme",
    get: async (path: string) => {
      if (path.endsWith("/credential/billing")) return handlers.billing();
      if (path.endsWith("/auth/me")) return { role: "member" };
      if (path.endsWith("/credential")) return handlers.credential();
      throw new Error(`unexpected GET ${path}`);
    },
  } as unknown as OpenCompanyClient;
}

/** An admin client, so the controls a member never sees are actually rendered.
 * `writes` collects every PUT body, which is how the confirmation tests tell a
 * cleared key from an offered one. */
function adminClient(
  credentialFor: () => Promise<CompanyCredentialStatus>,
  writes: unknown[] = [],
): OpenCompanyClient {
  return {
    scopeFor: () => "/api/v1/companies/acme",
    get: async (path: string) => {
      if (path.endsWith("/credential/billing")) return { configured: false };
      if (path.endsWith("/auth/me")) return { role: "admin" };
      if (path.endsWith("/credential")) return credentialFor();
      throw new Error(`unexpected GET ${path}`);
    },
    put: async (_path: string, body: unknown) => {
      writes.push(body);
      return { status: await credentialFor(), note: "" };
    },
  } as unknown as OpenCompanyClient;
}

/** Clicks a rendered control and lets the resulting state settle.
 *
 * Menus and dialogs render into a portal on `document.body`, not into the
 * container, so the queries below deliberately ask the document. */
async function press(selector: string) {
  const el = document.querySelector(selector);
  if (el === null) throw new Error(`nothing to press at ${selector}`);
  await act(async () => {
    (el as HTMLElement).click();
  });
  await act(async () => {});
}

async function mount(client: OpenCompanyClient) {
  await act(async () => {
    root.render(createElement(ApiKeyView, { client, company: "acme" }));
  });
  await act(async () => {});
  await act(async () => {});
}

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

describe("ApiKeyView billing failures stay distinguishable from no key", () => {
  // The regression this covers (CodeRabbit + Codex, PR #2216): the old catch
  // converted every billing rejection into `{ configured: false }`, so a
  // company that HAS a key saw the balance vanish entirely — the same outcome
  // as never having set one, even though `billing.unavailable` exists
  // precisely to say "the key is set, the hub just would not answer".
  it("keeps the balance row in its unavailable state when billing rejects but a key is configured", async () => {
    const client = clientFor({
      credential: async () => credential({ configured: true, source: "company" }),
      billing: async () => {
        throw new Error("network blip");
      },
    });

    await mount(client);

    const text = container.textContent ?? "";
    expect(container.querySelector('[data-testid="account-balance"]')).not.toBeNull();
    expect(text).toContain("Balance unknown");
    expect(text).toContain("The key is set");
    // Must NOT have fallen through to the empty state.
    expect(container.querySelector('[data-testid="account-empty"]')).toBeNull();
  });

  it("reports no account at all when billing rejects and nothing resolves", async () => {
    const client = clientFor({
      credential: async () => credential({ configured: false, source: "none" }),
      billing: async () => {
        throw new Error("network blip");
      },
    });

    await mount(client);

    // No balance row at all — nothing to be "unavailable" about.
    expect(container.querySelector('[data-testid="account-balance"]')).toBeNull();
    expect(container.querySelector('[data-testid="account-empty"]')).not.toBeNull();
    expect(container.textContent ?? "").toContain("No account connected yet.");
  });
});

describe("ApiKeyView describes a fallback platform identity honestly", () => {
  // Codex P2: a host with no company key but a live instance identity
  // (`attested` / `static`) already lets agents think and providers connect —
  // "agents cannot think and no app can be connected" is simply false there
  // and would send an operator to reconnect something that already works. The
  // row is keyed on what `resolve` returned, not on `configured`, which is
  // false in exactly this case.
  it("names the server's identity rather than saying nothing is configured", async () => {
    const client = clientFor({
      credential: async () => credential({ configured: false, source: "attested" }),
      billing: async () => ({ configured: false }),
    });

    await mount(client);

    const subline = container.querySelector('[data-testid="account-row-subline"]');
    expect(subline?.textContent).toBe("Acting as the account of whoever runs this server");
    expect(container.querySelector('[data-testid="account-empty"]')).toBeNull();
  });

  it("still says plainly when there is no identity at all", async () => {
    const client = clientFor({
      credential: async () => credential({ configured: false, source: "none" }),
      billing: async () => ({ configured: false }),
    });

    await mount(client);

    expect(container.querySelector('[data-testid="account-empty"]')).not.toBeNull();
    expect(container.textContent ?? "").toContain("No account connected yet.");
    expect(container.textContent ?? "").toContain("Apps cannot be connected");
  });
});

describe("ApiKeyView never overstates what a missing account breaks", () => {
  // QA, 2026-09-11. The old page said "Until one is set, agents cannot think
  // and no provider can be connected", and it is **false** on a company whose
  // LLM page holds a provider key of its own: `inference/key` resolves without
  // this credential, so such a company thinks perfectly well at `source:
  // "none"` — and the sentence sends its operator to fix something that is not
  // broken. The page may say what this key governs; it may not claim the whole
  // company has stopped.
  it("never claims agents cannot think, in any state", async () => {
    for (const source of ["none", "attested", "static", "company"] as const) {
      const client = clientFor({
        credential: async () => credential({ configured: source === "company", source }),
        billing: async () => ({ configured: false }),
      });

      await mount(client);

      const text = (container.textContent ?? "").toLowerCase();
      expect(text, `source=${source}`).not.toContain("agents cannot think");
      expect(text, `source=${source}`).not.toContain("cannot think");
    }
  });

  // The exception is named rather than denied — an operator who has set a
  // provider key on the LLM page must be able to see that it still applies.
  it("names the LLM-page provider key as the thing that still works", async () => {
    const client = clientFor({
      credential: async () => credential({ configured: false, source: "none" }),
      billing: async () => ({ configured: false }),
    });

    await mount(client);

    expect(container.textContent ?? "").toContain(
      "a provider key set on the LLM page still works",
    );
  });

  // The billing consequence belongs to the control it is true of. `PUT
  // …/credential` — what the paste dialog submits — writes `tinyhumans/key`
  // and stops; only `finish_link` also writes `inference/key` and declares the
  // managed provider. So the header card, which carries the Connect button,
  // states the move, and the dialog must not: telling someone that pasting a
  // key moved their model spend is the same defect pointing the other way.
  it("puts the billing move on the connect path, not on the paste field", async () => {
    await mount(
      adminClient(async () => credential({ configured: false, source: "none", hubLink: true })),
    );

    // The header card says it, beside the button it is true of.
    expect(container.textContent ?? "").toContain(
      "Connecting points both at this company's account",
    );
    // …and qualifies it, because the managed chain has rungs above this key.
    expect(container.textContent ?? "").toContain("which keeps precedence");
  });

  // The other half, and it has to open the dialog to be worth anything: with a
  // hub wired the header renders Connect and no paste field exists, so an
  // assertion against the closed page could not fail however the dialog were
  // worded. `hubLink: false` is the path that offers the field.
  it("says in the paste dialog that a paste sets the identity only", async () => {
    await mount(
      adminClient(async () => credential({ configured: false, source: "none", hubLink: false })),
    );

    await press('[data-testid="account-add-key"]');

    const dialog = document.body.textContent ?? "";
    expect(dialog).toContain("Pasting one sets the identity");
    expect(dialog).toContain("it does not choose a model provider");
    // `PUT …/credential` writes `tinyhumans/key` and stops; only `finish_link`
    // also declares the `managed` provider. What the field must not claim any
    // more is that it leaves the thinking alone — since #2266 a managed turn
    // resolves through this very key — so the dialog says that instead.
    expect(dialog).toContain("its turns resolve through this same key");
    expect(dialog).not.toContain("moves every agent turn");
  });
});

describe("ApiKeyView confirms before clearing a credential", () => {
  // QA matrix X8, and the standing rule behind it — an operator lost a live
  // key to an unconfirmed clear. `store_key("")` is how the store spells a
  // delete, it is irreversible from this console (the hub shows a key's value
  // once), and the menu item cannot show what it costs.
  it("offers Remove key as a confirmation, never as a direct write", async () => {
    const writes: unknown[] = [];
    await mount(adminClient(async () => credential({ source: "company" }), writes));

    // Mounting and rendering the row must never have written anything.
    expect(writes).toHaveLength(0);
    // At rest the menu is closed, so there is no one-press path to a cleared
    // key on the page at all.
    expect(document.querySelector('[data-testid="account-remove-key"]')).toBeNull();

    // Open it and press the destructive item. This is the press that used to
    // clear the key outright, and the assertion that matters is that it still
    // has not written anything.
    await press('[data-testid="account-row-menu"]');
    await press('[data-testid="account-remove-key"]');
    expect(writes).toHaveLength(0);
    expect(document.body.textContent ?? "").toContain("Remove this company's account key?");

    // Only the second, deliberate press reaches the host — and reaches it once,
    // with the empty value that is how the store spells a delete.
    await press('[data-testid="account-remove-key-confirm"]');
    expect(writes).toEqual([{ key: "" }]);
  });
});

describe("ApiKeyView never renders an unreadable store as an empty one", () => {
  // `company_key::resolve` propagates a secret-store read error rather than
  // falling through to the instance identity, because a connection made under
  // a silently-borrowed identity belongs to the wrong account invisibly and
  // permanently. The console has to spend a state on that, or the distinction
  // the host paid for is thrown away at the last step.
  it("says the host could not answer, and offers no empty state", async () => {
    const client = clientFor({
      credential: async () => {
        throw new Error("secret store unavailable");
      },
      billing: async () => ({ configured: false }),
    });

    await mount(client);

    const subline = container.querySelector('[data-testid="account-row-subline"]');
    expect(subline?.textContent).toContain("not the same as having no key");
    expect(container.querySelector('[data-testid="account-empty"]')).toBeNull();
    // And no balance under a row that has just said it does not know whose
    // account this is.
    expect(container.querySelector('[data-testid="account-balance"]')).toBeNull();
  });

  // The same honesty, applied to the controls rather than the words. An admin
  // reading "the host could not say" must not be offered a key field beside
  // it: the write it opens overwrites a write-only credential this console has
  // just admitted it cannot see, and the value it replaces cannot be read back
  // from the hub, which shows a key's plaintext once.
  it("offers no way to overwrite a key it cannot read", async () => {
    const client = {
      scopeFor: () => "/api/v1/companies/acme",
      get: async (path: string) => {
        if (path.endsWith("/credential/billing")) return { configured: false };
        if (path.endsWith("/auth/me")) return { role: "admin" };
        if (path.endsWith("/credential")) throw new Error("secret store unavailable");
        throw new Error(`unexpected GET ${path}`);
      },
    } as unknown as OpenCompanyClient;

    await mount(client);

    // The row is there, saying it does not know — that part is the point above.
    expect(container.querySelector('[data-testid="account-row"]')).not.toBeNull();
    // The header card's action is gone rather than disabled: there is no state
    // in which it is the right offer, so a greyed one would only invite a
    // retry.
    expect(container.querySelector('[data-testid="account-add-key"]')).toBeNull();
    // And the row menu, which carries the same "Add a key" item, cannot open.
    const menu = container.querySelector('[data-testid="account-row-menu"]');
    expect(menu).not.toBeNull();
    // Either spelling counts — the trigger is a `Button` rendered through the
    // menu primitive, and which of the two it forwards is the primitive's
    // business rather than this page's.
    const shut =
      menu?.hasAttribute("disabled") === true || menu?.getAttribute("aria-disabled") === "true";
    expect(shut).toBe(true);
  });
});

describe("ApiKeyView offers no control that cannot act", () => {
  // The rule that removed a toggle from the Managed inference row. Remove key
  // clears `tinyhumans/key`, which a company on the instance's identity does
  // not have — so offering it would be a destructive control that changes
  // nothing.
  it("does not offer Remove key when the identity is the instance's", async () => {
    const writes: unknown[] = [];
    // An **admin**, so the gate under test is the source rather than the role:
    // a member's menu is disabled whatever the tier, and asserting through one
    // would pass for the wrong reason.
    await mount(adminClient(async () => credential({ configured: false, source: "static" }), writes));

    await press('[data-testid="account-row-menu"]');

    // The menu opens — the admin gets the other item — and the destructive one
    // is simply not in it. `tinyhumans/key` is not what resolved here, so a
    // Remove would clear nothing.
    expect(document.body.textContent ?? "").toContain("Add a key");
    expect(document.querySelector('[data-testid="account-remove-key"]')).toBeNull();
    expect(writes).toHaveLength(0);
  });
});

describe("ApiKeyView redeems a returning grant whatever else failed", () => {
  // The grant comes back as a top-level navigation: `App` takes the code off
  // the URL before the first render, strips the address bar because it is a
  // live single-use credential, and hands it to a module-local box that a
  // reload empties. The effect inside `ConnectTinyHumansButton` is the only
  // thing that spends it.
  //
  // So that component's *mount* must not be gated on the credential read. It
  // was, briefly: the header offers one action and the connect action is
  // chosen from `status`, which is null while the read is in flight and stays
  // null when it fails — and a company whose secret store hiccuped on exactly
  // that page load would have lost the key it had just minted, with nothing on
  // screen to try again with. Only what is *shown* may depend on the read.
  it("finishes the link even when the credential read fails", async () => {
    const finished: unknown[] = [];
    captureKeyLink({ state: "st", code: "cd" }, false);

    const client = {
      scopeFor: () => "/api/v1/companies/acme",
      get: async (path: string) => {
        if (path.endsWith("/credential/billing")) return { configured: false };
        if (path.endsWith("/auth/me")) return { role: "admin" };
        if (path.endsWith("/credential")) throw new Error("secret store unavailable");
        throw new Error(`unexpected GET ${path}`);
      },
      post: async (path: string, body: unknown) => {
        finished.push({ path, body });
        return { status: credential({ source: "company" }), note: "" };
      },
    } as unknown as OpenCompanyClient;

    await mount(client);
    await act(async () => {});

    expect(finished).toHaveLength(1);
    expect((finished[0] as { path: string }).path).toContain("/credential/link/finish");
    expect((finished[0] as { body: unknown }).body).toEqual({ state: "st", code: "cd" });
  });

  // And the box is emptied by the redemption rather than by the mount, so a
  // page that never had a grant never calls the route.
  it("calls nothing when no grant is pending", async () => {
    const finished: unknown[] = [];
    captureKeyLink(null, false);

    const client = {
      scopeFor: () => "/api/v1/companies/acme",
      get: async (path: string) => {
        if (path.endsWith("/credential/billing")) return { configured: false };
        if (path.endsWith("/auth/me")) return { role: "admin" };
        if (path.endsWith("/credential")) return credential({ source: "company" });
        throw new Error(`unexpected GET ${path}`);
      },
      post: async () => {
        finished.push(true);
        return {};
      },
    } as unknown as OpenCompanyClient;

    await mount(client);
    await act(async () => {});

    expect(finished).toHaveLength(0);
  });
});
