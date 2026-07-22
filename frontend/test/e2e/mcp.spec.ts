import { expect, test } from "@playwright/test";

test("operator installs and calls an MCP server from the console and an agent", async ({
  page,
}) => {
  const serverScript = process.env.PW_MCP_SERVER;
  expect(
    serverScript,
    "PW_MCP_SERVER must point at the simple MCP server",
  ).toBeTruthy();

  await page.goto("/#/mcp");
  await page.getByTestId("mcp-install-name").fill("simple");
  await page.getByTestId("mcp-install-command").fill("node");
  await page.getByTestId("mcp-install-args").fill(serverScript!);
  await page.getByTestId("mcp-install-submit").click();

  const row = page.getByTestId("mcp-server-row").filter({ hasText: "simple" });
  await expect(row).toContainText("Connected", { timeout: 30_000 });
  await expect(row).toContainText("2 tools");

  const listed = await page.request.get("/api/v1/company/mcp/servers");
  expect(listed.ok()).toBeTruthy();
  const listedBody = await listed.json();
  const serverId = listedBody.servers.find(
    (server: { name: string }) => server.name === "simple",
  )?.server_id as string | undefined;
  expect(serverId).toBeTruthy();

  await row.getByRole("button", { name: "Show simple tools" }).click();
  const marker = `pw-${Date.now()}`;
  await row
    .getByTestId("mcp-tool-call-args")
    .fill(JSON.stringify({ text: marker }));
  await row.getByTestId("mcp-tool-call-run").click();
  await expect(row.getByTestId("mcp-tool-call-result")).toContainText(
    `echo: ${marker}`,
  );

  await page.goto("/#/conversation");
  const agentMarker = `agent-mcp-${Date.now()}`;
  const directive = `__MOCK_TOOL_CALL__ ${JSON.stringify({
    name: "mcp_registry_tool_call",
    arguments: {
      server_id: serverId,
      tool_name: "echo",
      arguments: { text: agentMarker },
    },
  })}`;
  await page.getByPlaceholder(/^Message /).fill(directive);
  await page.getByRole("button", { name: "Send" }).click();

  await expect(page.getByText(/__MOCK_LLM__/).last()).toBeVisible({
    timeout: 60_000,
  });
  await expect(
    page.getByText(new RegExp(`echo: ${agentMarker}`)).last(),
  ).toBeVisible();
  await expect(page.getByText(/^Couldn't send/)).toHaveCount(0);
});
