import { describe, expect, it } from "vitest";

import type { OpenCompanyClient } from "@/api/client";
import { fetchMailStatus } from "@/api/domain";
import { isValidDomain, parseSmtpPort } from "@/lib/domain";

/**
 * The custom-mail settings, now wired to the host (issue #1460).
 *
 * The card used to fabricate DNS records client-side and write the SMTP password
 * to localStorage on every keystroke. The wiring moved all of that to the host;
 * what stays on the client is validation and the shape of the status read, both
 * pinned here.
 */
describe("isValidDomain", () => {
  it("accepts a hostname with at least one dot", () => {
    expect(isValidDomain("mail.acme.com")).toBe(true);
    expect(isValidDomain("acme.co")).toBe(true);
  });

  it("rejects a bare label, a leading dash, and empty input", () => {
    expect(isValidDomain("localhost")).toBe(false);
    expect(isValidDomain("-acme.com")).toBe(false);
    expect(isValidDomain("")).toBe(false);
  });
});

describe("parseSmtpPort", () => {
  it("accepts a valid TCP port", () => {
    expect(parseSmtpPort("587")).toBe(587);
    expect(parseSmtpPort(" 465 ")).toBe(465);
    expect(parseSmtpPort("65535")).toBe(65535);
    expect(parseSmtpPort("1")).toBe(1);
  });

  it("rejects zero, out-of-range, and non-numeric — before the round trip", () => {
    // The host's port is a u16, so these must be caught here rather than
    // surfacing as an opaque deserialize failure.
    expect(parseSmtpPort("0")).toBeNull();
    expect(parseSmtpPort("70000")).toBeNull();
    expect(parseSmtpPort("587a")).toBeNull();
    expect(parseSmtpPort("")).toBeNull();
    expect(parseSmtpPort("-1")).toBeNull();
  });
});

/** A client whose only method fetchMailStatus needs is `graphqlRequest`. */
function clientReturning(result: { data?: unknown; errors?: unknown }): OpenCompanyClient {
  return {
    graphqlRequest: async () => result,
  } as unknown as OpenCompanyClient;
}

describe("fetchMailStatus", () => {
  it("returns the domain and smtp status the host reports", async () => {
    const client = clientReturning({
      data: {
        company: {
          domain: {
            domain: "mail.acme.com",
            verified: true,
            records: [{ type: "TXT", name: "_oc.mail.acme.com", value: "v=1", ttl: "3600" }],
          },
          smtp: { host: "smtp.acme.com", port: 587, username: "apikey", configured: true },
        },
      },
    });
    const status = await fetchMailStatus(client, "acme");
    expect(status.domain?.domain).toBe("mail.acme.com");
    expect(status.domain?.records).toHaveLength(1);
    expect(status.smtp.configured).toBe(true);
    expect(status.smtp.host).toBe("smtp.acme.com");
  });

  it("reads a null domain as 'none set', and a missing smtp as unconfigured", async () => {
    const client = clientReturning({ data: { company: { domain: null, smtp: null } } });
    const status = await fetchMailStatus(client, null);
    expect(status.domain).toBeNull();
    expect(status.smtp).toEqual({ host: "", port: 0, username: "", configured: false });
  });

  it("throws on a GraphQL error rather than reporting empty state as fact", async () => {
    // The card turns this into a "couldn't read" notice — an unanswered read is
    // not "nothing configured".
    const client = clientReturning({ errors: [{ message: "boom" }] });
    await expect(fetchMailStatus(client, "acme")).rejects.toThrow(/boom/);
  });
});
