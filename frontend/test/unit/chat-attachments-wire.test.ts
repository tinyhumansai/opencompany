// @vitest-environment jsdom

import { afterEach, describe, expect, it, vi } from "vitest";

import { uploadChatAttachment } from "@/api/chat";
import { OpenCompanyClient } from "@/api/client";

/**
 * Issue #1682: the chat-attachment wire contract — what the send path puts on
 * the body, and what the upload path posts.
 *
 * The security posture is "ids only": the client sends node ids and the host
 * re-resolves them, so these pin that the body carries ids and never the
 * store-owned name / mime / size, and that an empty list keeps the pre-#1682
 * body shape byte-for-byte.
 */

/** A client whose transport records the JSON body of every request. */
function recordingClient() {
  const bodies: Array<Record<string, unknown>> = [];
  const transport = {
    request: async (req: { method: string; url: string; body?: string }) => {
      bodies.push(req.body === undefined ? {} : JSON.parse(req.body));
      return {
        status: 200,
        statusText: "OK",
        url: req.url,
        text: JSON.stringify({ responses: [] }),
        header: () => null,
      };
    },
    subscribe: () => () => {},
  };
  const client = new OpenCompanyClient(
    { baseUrl: "", company: "acme", operatorToken: "t0ken", sessionHeader: null },
    transport as never,
  );
  return { client, bodies };
}

describe("client.chat — attachments on the wire", () => {
  it("carries the node ids when the message has attachments", async () => {
    const { client, bodies } = recordingClient();
    await client.chat("see this", "acme", null, null, undefined, false, ["n1", "n2"]);
    expect(bodies[0]).toEqual({ text: "see this", attachments: ["n1", "n2"] });
  });

  it("omits the key entirely when there are no attachments — the pre-#1682 shape", async () => {
    const { client, bodies } = recordingClient();
    await client.chat("hi", "acme", null, null, undefined, false, []);
    expect(bodies[0]).toEqual({ text: "hi" });
    await client.chat("hi again", "acme");
    expect(bodies[1]).not.toHaveProperty("attachments");
  });
});

describe("uploadChatAttachment", () => {
  afterEach(() => vi.unstubAllGlobals());

  it("posts the file as multipart to /chat/upload and returns the store reference", async () => {
    const seen: { url?: string; method?: string; body?: unknown } = {};
    vi.stubGlobal(
      "fetch",
      vi.fn(async (url: string, init: RequestInit) => {
        seen.url = url;
        seen.method = init.method;
        seen.body = init.body;
        return new Response(
          JSON.stringify({ nodeId: "node-9", name: "hero.png", mime: "image/png", size: 42 }),
          { status: 200, headers: { "content-type": "application/json" } },
        );
      }),
    );
    const client = new OpenCompanyClient({
      baseUrl: "",
      company: "acme",
      operatorToken: "t0ken",
      sessionHeader: null,
    });
    const file = new File([new Uint8Array([1, 2, 3])], "hero.png", { type: "image/png" });

    const reference = await uploadChatAttachment(client, "acme", file);

    expect(seen.method).toBe("POST");
    expect(seen.url).toContain("/chat/upload");
    expect(seen.body).toBeInstanceOf(FormData);
    expect((seen.body as FormData).get("file")).toBeInstanceOf(File);
    // Every field is the store's, straight off the response — nothing invented.
    expect(reference).toEqual({ nodeId: "node-9", name: "hero.png", mime: "image/png", size: 42 });
  });
});
