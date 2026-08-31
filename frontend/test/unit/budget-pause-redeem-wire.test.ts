// @vitest-environment jsdom

import { describe, expect, it } from "vitest";

import { OpenCompanyClient } from "@/api/client";

/**
 * Issue #1846 review (Codex #3866418876 / #3866802268) — the `?id=` wire
 * contract `redeemBudgetPause` must honour.
 *
 * The server's `redeem_matching` path (issue #1846 review, Codex
 * #3866418876) only protects the console from redeeming a stale marker when
 * the request actually carries the marker id it last read — an omitted `id`
 * falls back to the server's pre-fix unconditional redeem. Before this fix,
 * `ChatView`'s "Add credits" CTA never read the marker back and this method
 * never appended `?id=` at all, so the atomic mismatch check on the server
 * had nothing to compare against and could never fire from the console.
 * These pin the URL shape directly, since there is no component-test harness
 * in this project to mount `ChatView`'s click handler (see
 * `budget-pause-notice.test.ts`'s header doc comment for the same
 * constraint).
 */

/** A client whose transport records the URL of every request. */
function recordingClient() {
  const urls: string[] = [];
  const transport = {
    request: async (req: { method: string; url: string }) => {
      urls.push(req.url);
      return {
        status: 200,
        statusText: "OK",
        url: req.url,
        text: JSON.stringify({
          id: "marker-1",
          agent: "ceo",
          message: "ship the API",
          summary: "paused",
          atMillis: 1_000,
        }),
        header: () => null,
      };
    },
    subscribe: () => () => {},
  };
  const client = new OpenCompanyClient(
    { baseUrl: "", company: "acme", operatorToken: "t0ken", sessionHeader: null },
    transport as never,
  );
  return { client, urls };
}

describe("client.redeemBudgetPause — the ?id= wire contract", () => {
  it("appends ?id= when the caller passes the marker id it read back", async () => {
    const { client, urls } = recordingClient();
    await client.redeemBudgetPause("ceo", "acme", "marker-1");
    expect(urls[0]).toBe(
      "/api/v1/companies/acme/agents/ceo/budget-pause/redeem?id=marker-1",
    );
  });

  it("URL-encodes an id that needs it", async () => {
    const { client, urls } = recordingClient();
    await client.redeemBudgetPause("ceo", "acme", "marker/with space");
    expect(urls[0]).toBe(
      "/api/v1/companies/acme/agents/ceo/budget-pause/redeem?id=marker%2Fwith%20space",
    );
  });

  it("omits ?id= entirely when no id is given — the pre-fix shape a caller with nothing to compare against still needs", async () => {
    const { client, urls } = recordingClient();
    await client.redeemBudgetPause("ceo", "acme");
    expect(urls[0]).toBe("/api/v1/companies/acme/agents/ceo/budget-pause/redeem");
  });

  it("omits ?id= when the caller explicitly passes null — the 'read back, found nothing' case", async () => {
    const { client, urls } = recordingClient();
    await client.redeemBudgetPause("ceo", "acme", null);
    expect(urls[0]).toBe("/api/v1/companies/acme/agents/ceo/budget-pause/redeem");
  });
});

describe("client.getBudgetPause — the read-back the CTA now performs first", () => {
  it("hits the read-only GET route, not the redeem route", async () => {
    const { client, urls } = recordingClient();
    await client.getBudgetPause("ceo", "acme");
    expect(urls[0]).toBe("/api/v1/companies/acme/agents/ceo/budget-pause");
  });
});
