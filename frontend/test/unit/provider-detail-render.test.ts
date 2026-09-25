// @vitest-environment jsdom

import { act, createElement } from "react";
import { flushSync } from "react-dom";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it } from "vitest";

import type { OpenCompanyClient } from "@/api/client";
import type {
  ComposioConnectedAccount,
  ComposioToolkitEntry,
} from "@/api/composio";
import type { ConnectionState, UsageDto } from "@/api/types";
import type { ComposioReach } from "@/lib/connections";
import { buildGridProviders, type GridProvider } from "@/lib/provider-grid";

const { ProviderDetail } = await import("@/views/connections/ProviderDetail");
type ConnectionSubject =
  import("@/views/connections/ProviderDetail").ConnectionSubject;

/**
 * The connection detail view's claims (issues #404, #821).
 *
 * This suite is normally for pure functions — see `vitest.config.ts`. The
 * exception is earned the same way `select-popup-width` earns it: the thing
 * under test *is* what reaches the operator's eye. The issue is not asking for
 * fields on a panel, it is asking that every sentence on the panel be one the
 * system can back, and four of them cannot be checked anywhere but here:
 *
 *  1. no account is marked as the one agents use, because none is;
 *  2. a missing connection date says so rather than showing a blank;
 *  3. a member is not offered a disconnect the host will refuse (#403);
 *  4. an MCP server's usage is read under `mcp:<server>` and never as the
 *     same-named Composio toolkit's (#698, #821).
 */

const OPEN: ComposioReach = {
  inBuild: true,
  granted: true,
  hasCredential: true,
  openMode: true,
  effectiveToolkits: [],
};

const EMPTY_USAGE: UsageDto = {
  totals: {
    inputTokens: 0,
    outputTokens: 0,
    tokens: 0,
    costUsd: 0,
    oauthCalls: 0,
    connections: 0,
    searchCalls: 0,
  },
  series: [],
  byAgent: [],
  byProvider: [],
};

function entry(slug: string, name: string): ComposioToolkitEntry {
  return { slug, name, description: "", logo: null, categories: [] };
}

function account(
  over: Partial<ComposioConnectedAccount> = {},
): ComposioConnectedAccount {
  return { id: "conn-1", status: "ACTIVE", connected: true, ...over };
}

/** A real grid row, built the way the page builds it. */
function gmail(
  accounts: ComposioConnectedAccount[],
  via: ConnectionState["via"] = ["composio"],
) {
  const rows = buildGridProviders(
    [entry("gmail", "Gmail")],
    [],
    { gmail: { provider: "gmail", connected: true, via } },
    OPEN,
    false,
    { gmail: accounts },
  );
  return rows.find((p) => p.slug === "gmail")!;
}

/** The same provider, connected to nothing. */
function unconnectedGmail() {
  const rows = buildGridProviders(
    [entry("gmail", "Gmail")],
    [],
    {},
    OPEN,
    false,
  );
  return rows.find((p) => p.slug === "gmail")!;
}

/** A host that answers the usage route with `byProvider` rows. */
function clientWith(byProvider: UsageDto["byProvider"]): OpenCompanyClient {
  return {
    usage: async () => ({ ...EMPTY_USAGE, byProvider }),
  } as unknown as OpenCompanyClient;
}

let container: HTMLDivElement;
let root: Root;

async function render(
  provider: GridProvider,
  canManage: boolean,
  client = clientWith([]),
  noCredential = false,
) {
  await open(
    {
      kind: "composio",
      provider,
      noCredential,
      onConnectAnother: () => {},
      onDisconnectAccount: () => {},
    },
    canManage,
    client,
  );
}

/** Open the panel on any subject — the shared half of `render` and `openMcp`. */
async function open(
  subject: ConnectionSubject,
  canManage: boolean,
  client = clientWith([]),
) {
  await act(async () => {
    root.render(
      createElement(ProviderDetail, {
        client,
        company: null,
        subject,
        canManage,
        busy: false,
        onClose: () => {},
      }),
    );
  });
}

/** The panel renders through a portal, so it is on `document`, not `container`. */
function text(): string {
  return document.body.textContent ?? "";
}

beforeEach(() => {
  (
    globalThis as unknown as { IS_REACT_ACT_ENVIRONMENT: boolean }
  ).IS_REACT_ACT_ENVIRONMENT = true;
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
});

describe("the provider detail view", () => {
  it("lists every account with the status Composio reported", async () => {
    await render(
      gmail([
        account({ id: "conn-gmail-1", account: "ops@acme.test" }),
        account({
          id: "conn-gmail-2",
          account: "billing@acme.test",
          status: "EXPIRED",
          connected: false,
        }),
      ]),
      true,
    );
    expect(text()).toContain("ops@acme.test");
    expect(text()).toContain("billing@acme.test");
    // Verbatim, not re-spelled: "set up and since expired" and "never finished
    // setting up" are different sentences that both flatten to "not connected".
    expect(text()).toContain("EXPIRED");
  });

  it("marks no account itself, and points at where the choice is made", async () => {
    // OpenHuman marks the first of several as the default and inheriting that
    // was the plan. #819 refused, correctly for its own moment: `composio_execute`
    // posted `{tool, arguments}` and no connection id, so a "Default" chip would
    // have named a decision the product did not make.
    //
    // #820 makes the decision real, which is what this assertion had to change
    // for. The panel still marks nothing — the choice is one control on one
    // surface, and a second place to read it back is how two surfaces come to
    // disagree — but it no longer tells the operator the choice does not exist,
    // because on this page it now does.
    await render(
      gmail([
        account({ id: "conn-gmail-1", account: "ops@acme.test" }),
        account({ id: "conn-gmail-2", account: "billing@acme.test" }),
      ]),
      true,
    );
    expect(text()).not.toContain("Default");
    expect(text()).toContain("Which account agents act as");
    // And specifically not the claim it replaced: an operator reading this
    // panel must not be told to disconnect an account to control which one acts.
    expect(text()).not.toContain("sends no connection id");
  });

  it("says a connection date is not recorded rather than leaving it blank", async () => {
    await render(gmail([account({ account: "ops@acme.test" })]), true);
    expect(text()).toContain("connection date not recorded");
  });

  it("states usage is counted per provider, not per account", async () => {
    await render(
      gmail([
        account({ id: "conn-gmail-1", account: "ops@acme.test" }),
        account({ id: "conn-gmail-2", account: "billing@acme.test" }),
      ]),
      true,
      clientWith([{ provider: "gmail", calls: 12 }]),
    );
    expect(text()).toContain("12");
    expect(text()).toContain("per provider rather than per account");
  });

  it("does not report a zero when the host records no usage at all", async () => {
    // A failed usage read is not a company that made no calls. Rendering "0"
    // here is precisely the plausible-looking zero the issue forbids.
    const broken = {
      usage: async () => {
        throw new Error("no usage route on this host");
      },
    } as unknown as OpenCompanyClient;
    await render(gmail([account()]), true, broken);
    expect(text()).toContain("does not report usage");
    expect(text()).not.toContain("0 calls");
  });

  it("says what a disconnect will and will not revoke", async () => {
    await render(gmail([account({ account: "ops@acme.test" })]), true);
    expect(text()).toContain("removes the connection at Composio");
    expect(text()).toContain("does not sign the company out");
  });

  it("offers a member no control the host would refuse", async () => {
    // Issue #403. Courtesy, not enforcement — the host answers 403 whatever
    // this renders — but a Disconnect that can only fail is a poor thing to
    // put in front of someone reading the page to find out what is wired.
    await render(gmail([account({ account: "ops@acme.test" })]), false);
    expect(text()).toContain("ops@acme.test");
    expect(text()).not.toContain("Disconnect");
    expect(text()).not.toContain("Connect another account");
    expect(text()).toContain("Only an admin can connect or disconnect");
  });

  it("opens on a provider that is not connected, and offers the connect", async () => {
    // "connected **or not**" is the issue's wording, and OpenHuman's modal is a
    // phase machine that opens on a disconnected toolkit and connects from
    // inside. Keeping the panel for connected providers only was the earlier
    // cut here; it answered "what is wired" and not "what is this".
    await render(unconnectedGmail(), true);
    expect(text()).toContain("not connected");
    expect(text()).toContain("Connect an account");
    expect(text()).toContain("agents have none of its tools");
  });

  it("shows no usage figure for a provider that was never connected or used", async () => {
    // A true zero that says nothing, over a catalog of 123 untouched providers.
    await render(unconnectedGmail(), true);
    expect(text()).not.toContain("in the last 30 days");
  });

  it("keeps the usage figure on a disconnected provider that was used", async () => {
    // The interesting case: the calls happened, then the account went away.
    await render(
      unconnectedGmail(),
      true,
      clientWith([{ provider: "gmail", calls: 7 }]),
    );
    expect(text()).toContain("7");
    expect(text()).toContain("in the last 30 days");
  });

  it("says why a connect cannot be started when no credential resolves", async () => {
    // Stated where the button is, not only on the page behind the panel.
    await render(unconnectedGmail(), true, clientWith([]), true);
    expect(text()).toContain(
      "no credential for this company to authorize against",
    );
  });

  it("names the inert native credential as a caveat, not as a second connection", async () => {
    // #396: the native `oauth/{provider}` entry is written and read by nothing.
    // A detail view is where that would look healthiest, so it is stated.
    await render(gmail([account()], ["composio", "native"]), true);
    expect(text()).toContain("No agent reads it");
  });
});

/** A server as `.../mcp/servers` returns it. */

/** A host whose usage answer is released by the test, one call at a time. */
function gatedClient(byProvider: UsageDto["byProvider"]) {
  const gates: Array<() => void> = [];
  const client = {
    usage: () =>
      new Promise<UsageDto>((resolve) => {
        gates.push(() => resolve({ ...EMPTY_USAGE, byProvider }));
      }),
  } as unknown as OpenCompanyClient;
  return { client, gates };
}

/** The Usage section's own text, so a figure cannot be matched from elsewhere. */
function usageText(): string {
  return (
    document.querySelector('[data-testid="connection-detail-usage"]')
      ?.textContent ?? ""
  );
}

/** A connected Composio provider by slug, with one account. */
function connected(slug: string, name: string): GridProvider {
  const rows = buildGridProviders(
    [entry(slug, name)],
    [],
    { [slug]: { provider: slug, connected: true, via: ["composio"] } },
    OPEN,
    false,
    { [slug]: [account()] },
  );
  return rows.find((p) => p.slug === slug)!;
}

describe("the same panel, changed from one subject to another", () => {
  it("never shows one provider's call count under another provider's name", async () => {
    // The sheet stays mounted and changes subject, so the usage figure is state
    // that outlives the thing it describes. State reset in an effect lands one
    // render *after* the new subject does — long enough for the browser to
    // paint 4,242 Gmail calls against Slack's name, which is not a slow render
    // but a wrong claim.
    const { client, gates } = gatedClient([
      { provider: "gmail", calls: 4242 },
      { provider: "slack", calls: 7 },
    ]);

    await render(connected("gmail", "Gmail"), true, client);
    await act(async () => gates[0]());
    expect(text()).toContain("4242");

    // Change subject and read the DOM *before* effects run — the frame the
    // operator sees. The second usage read is deliberately left in flight, so
    // nothing can have answered for Slack yet.
    //
    // Deliberately outside `act`, which flushes effects before it returns and
    // would hide the very frame under test. The environment flag is dropped for
    // the duration only so React does not warn about the render it is being
    // asked to do.
    const env = globalThis as unknown as { IS_REACT_ACT_ENVIRONMENT: boolean };
    env.IS_REACT_ACT_ENVIRONMENT = false;
    flushSync(() => {
      root.render(
        createElement(ProviderDetail, {
          client,
          company: null,
          subject: {
            kind: "composio",
            provider: connected("slack", "Slack"),
            noCredential: false,
            onConnectAnother: () => {},
            onDisconnectAccount: () => {},
          },
          canManage: true,
          busy: false,
          onClose: () => {},
        }),
      );
    });
    env.IS_REACT_ACT_ENVIRONMENT = true;
    expect(text()).toContain("Slack");
    expect(text()).not.toContain("4242");
    expect(text()).toContain("Reading usage…");

    // The frame under test is behind us, so the effect that render scheduled
    // can be flushed — deliberately, rather than trusting it to have run by the
    // scheduler's grace, since `gates[1]` does not exist until it has.
    await act(async () => {});
    expect(
      gates,
      "the subject change must have issued a usage read of its own",
    ).toHaveLength(2);

    // And Slack's own answer, when it arrives, is Slack's. Read from the Usage
    // section itself: a bare `7` would be satisfied by any digit anywhere on a
    // panel that also carries account counts and timestamps.
    await act(async () => gates[1]());
    expect(usageText()).toContain("7 calls in the last 30 days");
    expect(text()).not.toContain("4242");
  });
});
