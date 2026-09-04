import { describe, expect, it } from "vitest";

import type { OpenCompanyClient } from "@/api/client";
import { runQuery } from "@/api/graphql";
import { ApiError } from "@/api/types";

/** A client that records what it was asked for and replays a canned answer. */
function stub(response: { data?: unknown; errors?: unknown }) {
  const calls: (string | null | undefined)[] = [];
  const client = {
    graphqlRequest: async (
      _query: string,
      _variables?: Record<string, unknown>,
      company?: string | null,
    ) => {
      calls.push(company);
      return response;
    },
  } as unknown as OpenCompanyClient;
  return { client, calls };
}

/** Runs a document expected to be refused, and hands back the typed error. */
async function refusalFrom(client: OpenCompanyClient): Promise<ApiError> {
  try {
    await runQuery(client, "{ company { id } }");
  } catch (err) {
    expect(err).toBeInstanceOf(ApiError);
    return err as ApiError;
  }
  throw new Error("the query resolved when it should have been refused");
}

describe("the console's GraphQL entry point", () => {
  it("carries the addressed company to the client", async () => {
    const { client, calls } = stub({ data: { company: { id: "acme" } } });

    await expect(runQuery(client, "{ company { id } }", { company: "acme" }, "acme")).resolves.toEqual({
      company: { id: "acme" },
    });
    // The company travels beside the document, not only inside it: the host's
    // auth layer runs before the body is read.
    expect(calls).toEqual(["acme"]);
  });

  it("turns a refusal into a sentence and marks it unretryable", async () => {
    const { client } = stub({ data: null, errors: [{ message: "unauthorized" }] });

    const err = await refusalFrom(client);
    expect(err.code).toBe("graphql_refused");
    // The wire token must not reach the panel.
    expect(err.message).not.toBe("unauthorized");
    expect(err.message).toMatch(/sign in again/i);
  });

  it("names a forbidden read as a permission problem", async () => {
    const { client } = stub({ data: null, errors: [{ message: "forbidden" }] });

    const err = await refusalFrom(client);
    expect(err.code).toBe("graphql_refused");
    expect(err.message).toMatch(/isn't visible to your account/i);
  });

  it("leaves an ordinary resolver failure alone and retryable", async () => {
    const { client } = stub({ data: null, errors: [{ message: "the store timed out" }] });

    const err = await refusalFrom(client);
    expect(err.code).toBe("graphql_error");
    expect(err.message).toBe("the store timed out");
  });
});
