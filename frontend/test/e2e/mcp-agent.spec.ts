import { randomUUID } from "node:crypto";

import { expect, test, type Page } from "@playwright/test";

import { LIVE_BRAIN, LIVE_BRAIN_REASON, MCP_SERVER } from "./capabilities";

/**
 * The half of the MCP bridge a console cannot reach: an **agent** calling a
 * tool on a real server, over the real transport (issues #50, #467).
 *
 * `mcp.spec.ts` covers the operator's side — Settings, MCP Servers lists what
 * the host serves, and an admin adds and removes a runtime server — against a
 * default-feature host, because none of that needs an agent. Everything past
 * the registry does: `mcp_call_tool` is on an agent's belt only under the `mcp`
 * feature, and something has to decide to call it. So this spec lives in the
 * live-brain lane and is the only place the whole path is exercised:
 *
 *   agent turn → tool belt → registry → HTTP → MCP server → tool result →
 *   the agent's own reply
 *
 * # What is a fixture, and what is not
 *
 * Two fixtures, both local and neither faking a boundary that matters. The MCP
 * server is `mcp-server.mjs` — two tools, no network, speaking the JSON-RPC the
 * vendored transport drives. And the inference backend's *choice* of tool is
 * scripted: `__MOCK_TOOL_CALL__` makes the model call one named tool, because a
 * spec cannot assert on a model that is free to decline. The registry, the
 * transport, the belt, the approval policy and the journal are all real.
 *
 * # The tool is `mcp_call_tool`
 *
 * Not upstream OpenHuman's `mcp_registry_tool_call`, which is what this file's
 * predecessor asked for and never ran to find out. The belt this host builds
 * carries OpenCompany's own decorator (`src/harness/build.rs`), whose schema is
 * `{server, tool, arguments}` and which addresses a server by NAME.
 */

// The fixture is the whole subject: without a server there is nothing to call.
test.skip(
  !MCP_SERVER,
  "needs PW_MCP_SERVER pointing at an HTTP MCP server. The `Console E2E " +
    "(live brain)` CI lane starts one (issue #467).",
);
test.skip(!LIVE_BRAIN, LIVE_BRAIN_REASON);

/**
 * Opens the conversation view on the company thread.
 *
 * The thread is SELECTED, not merely navigated to. A composer is present either
 * way, so a `fill` succeeds — but the reply then lands in a transcript this page
 * is not showing. Scoped to the chat list because the sidebar's company switcher
 * is also a button carrying the company name.
 */
async function openThread(page: Page) {
  await page.goto("/#/conversation");
  const skip = page.getByRole("button", { name: "Skip for now" });
  await skip
    .waitFor({ state: "visible", timeout: 5_000 })
    .then(() => skip.click())
    .catch(() => {
      /* already dismissed in this context */
    });
  await page
    .getByRole("complementary")
    .getByRole("button", { name: /Your company/ })
    .first()
    .click();
}

/**
 * A row of the open transcript carrying `text`.
 *
 * Both selectors, because the two chat surfaces draw a message differently —
 * the Conversation view wraps each in `div.group/msg`, the Chat tab in an
 * `article[data-message-id]` — and this spec should not fail merely because it
 * was pointed at the other one.
 */
function transcriptRow(page: Page, text: string) {
  return page
    .locator("div.group\\/msg, article[data-message-id]")
    .filter({ hasText: text })
    .last();
}

test("an agent calls a tool on a registered MCP server and shows the result", async ({
  page,
}) => {
  // Unique per run, and a 409 is a FAILURE rather than a shrug. The host keeps
  // runtime servers in its secret store, so a fixed name plus a tolerated
  // "already exists" would let this spec adopt a leftover registration from an
  // earlier run — pointing anywhere at all — pass against it, and then delete a
  // server it never created.
  const server = `pw-agent-mcp-${randomUUID()}`;

  // Registered through the API rather than the console: the console's add form
  // is `mcp.spec.ts`'s subject, and repeating it here would make this spec fail
  // for that page's reasons rather than its own.
  const added = await page.request.post("/api/v1/company/mcp/servers", {
    data: { name: server, endpoint: MCP_SERVER!, description: "e2e fixture" },
  });
  expect(
    added.ok(),
    `registering ${server} failed: ${added.status()} ${await added.text()}`,
  ).toBeTruthy();

  let bodyPassed = false;
  try {
    await openThread(page);

    const marker = `agent-mcp-${randomUUID()}`;
    const directive = `__MOCK_TOOL_CALL__ ${JSON.stringify({
      name: "mcp_call_tool",
      arguments: { server, tool: "echo", arguments: { text: marker } },
    })}`;

    // The POST is awaited EXPLICITLY, and the reload below is why. A turn runs
    // inside the request that started it and the host drops the work when the
    // client goes away, so reloading while the send is in flight cancels the
    // turn before it reaches the model. Observed, not feared: on the run that
    // first reloaded here, the mock backend logged no call at all for this
    // message where the run before it had logged the whole round trip.
    const posted = page.waitForResponse(
      (response) => response.url().endsWith("/chat") && response.request().method() === "POST",
      { timeout: 90_000 },
    );
    await page.getByPlaceholder(/^Message /).fill(directive);
    await page.getByRole("button", { name: "Send" }).click();
    await expect(page.getByText(/^Couldn't send/)).toHaveCount(0);
    expect((await posted).ok(), "the chat POST did not succeed").toBeTruthy();

    // Read the answer from a RELOADED transcript: it is then rehydrated from
    // `chat/history`, so what is asserted is the durable record of the turn
    // rather than whatever the open view chose to draw — the same move
    // `chat-to-card.spec.ts` makes, and the stronger claim.
    await page.reload();
    await openThread(page);

    // Both halves of the round trip on one row: the remote tool's own output,
    // which can only have come from the fixture over HTTP, and the marker that
    // says the mocked backend produced the reply carrying it.
    //
    // `MOCK_LLM`, not `__MOCK_LLM__`: a bubble renders its text as markdown and
    // the marker's own underscores are emphasis syntax, so what reaches the DOM
    // is `MOCK_LLM` inside a `<strong>`. Only the plain-text surfaces — the
    // rail's thread preview, an API response — carry it verbatim, which is a
    // tidy way to assert against the sidebar by accident.
    const reply = transcriptRow(page, `echo: ${marker}`);
    await expect(reply).toBeVisible({ timeout: 30_000 });
    await expect(reply).toContainText("MOCK_LLM");
    bodyPassed = true;
  } finally {
    // The host persists runtime servers in its secret store, so a spec that
    // failed half way would otherwise leave this one registered for every later
    // run against the same data root.
    //
    // NOTHING here may throw past a failing body. An exception raised in a
    // `finally` replaces the error already travelling out of the `try`, so a
    // cleanup complaint would erase the real failure — the harder of the two to
    // debug, and the one worth keeping. That covers both ways this can go
    // wrong: `page.request.delete` REJECTS on a transport failure (it does not
    // return a response to inspect), and the status check is an assertion.
    // Both are therefore reported only when the body itself passed.
    let removed: Awaited<ReturnType<typeof page.request.delete>> | undefined;
    let transportError: unknown;
    try {
      removed = await page.request.delete(`/api/v1/company/mcp/servers/${server}`);
    } catch (error) {
      transportError = error;
    }
    if (bodyPassed) {
      if (transportError) throw transportError;
      expect(
        removed!.ok(),
        `removing ${server} failed: ${removed!.status()} ${await removed!.text()}`,
      ).toBeTruthy();
    }
  }
});
