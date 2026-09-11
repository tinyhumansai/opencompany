// Where the TinyHumans account links belong, now that the LLM page is a list.
//
// The property these tests were written for (CodeRabbit, PR #2216) was that the
// links must key off `credential.configured` rather than
// `status.keyConfigured`, and must not appear once the saved config no longer
// rides the platform proxy. Under `managed` — a legacy alias for `openrouter` —
// `keyConfigured` can be a raw OpenRouter key the operator pasted directly, and
// that key bills OpenRouter rather than the TinyHumans account the links point
// at.
//
// They were pinned by mounting the single-provider form, which rendered
// `HubAccountLinks` inline. **That form is retired**, and the links are not on
// the LLM page at all any more: the Connections rail groups Account and LLM as
// two rows under API Keys (#2259), and "top up the account this company spends
// through" belongs on the account row rather than beside a list of vendors that
// are mostly not TinyHumans.
//
// So the condition the old tests pinned is now enforced by *absence*, and that
// is what is pinned here — plus the component's own rule, tested directly
// rather than through a page, since `CompanyCredentialCard` is now its only
// caller and it was the one caller that was always right.

import { describe, expect, it } from "vitest";
import { renderToStaticMarkup } from "react-dom/server";
import { createElement } from "react";

import { HubAccountLinks } from "@/views/connections/HubAccountLinks";

const ACCOUNT = { manageKeysUrl: "https://hub.example/keys", topUpUrl: "https://hub.example/topup" };

function render(props: Parameters<typeof HubAccountLinks>[0]) {
  return renderToStaticMarkup(createElement(HubAccountLinks, props));
}

describe("HubAccountLinks", () => {
  it("renders nothing at all when the host sent no account", () => {
    // A host on a backend the naming convention does not describe sends no
    // links, and this renders nothing rather than guessing an origin — a link
    // built in the browser would send an operator to production's billing page
    // to wonder where their top-up went.
    expect(render({ account: undefined, configured: true })).toBe("");
  });

  it("says the balance is being spent only when the account key is configured", () => {
    // `configured` is the **TinyHumans credential**, never the inference key.
    // Keying off the inference key rendered the "not yet configured" copy for
    // an account that in fact had one — and the reverse, on a company whose
    // inference key was a raw OpenRouter key billing somebody else entirely.
    expect(render({ account: ACCOUNT, configured: true })).toContain(
      "Your agents spend this account&#x27;s balance",
    );
    expect(render({ account: ACCOUNT, configured: false })).toContain(
      "Already have a key, or need to add funds first?",
    );
  });

  it("links to the URLs the host sent, verbatim", () => {
    const html = render({ account: ACCOUNT, configured: true });
    expect(html).toContain(ACCOUNT.manageKeysUrl);
    expect(html).toContain(ACCOUNT.topUpUrl);
  });
});

describe("the LLM page does not carry them", () => {
  it("has no reference to the hub account links", async () => {
    // The enforcement is structural: if a later change renders them back onto
    // this page, the condition the old tests protected — hide them once the
    // config no longer rides the proxy — comes back with it, unpinned.
    const sources = await Promise.all(
      [
        import("@/inference/ProvidersTab?raw"),
        import("@/inference/RoutingTab?raw"),
        import("@/inference/ProviderList?raw"),
      ].map((p) => p.then((m) => (m as { default: string }).default)),
    );
    for (const source of sources) {
      expect(source).not.toContain("HubAccountLinks");
    }
  });
});
