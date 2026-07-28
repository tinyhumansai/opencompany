import { expect, request, type FullConfig } from "@playwright/test";

const ADMIN_EMAIL = "harness-e2e@tinyhumans.ai";

export default async function globalSetup(config: FullConfig) {
  const storageState = process.env.PW_STORAGE_STATE;
  if (!storageState) return;

  const baseURL = config.projects[0]?.use.baseURL as string | undefined;
  const context = await request.newContext({ baseURL });
  try {
    const requested = await context.post("/api/v1/company/auth/request", {
      data: { email: ADMIN_EMAIL },
    });
    expect(requested.ok()).toBeTruthy();
    const requestedBody = await requested.json();
    const devCode = requestedBody.dev_code as string | undefined;
    expect(
      devCode,
      "auth/request must echo dev_code on a loopback bind",
    ).toBeTruthy();

    const verified = await context.post("/api/v1/company/auth/verify", {
      data: { code: devCode },
    });
    expect(
      verified.ok(),
      "auth/verify must accept the dev_code and set a session",
    ).toBeTruthy();
    await context.storageState({ path: storageState });
  } finally {
    await context.dispose();
  }
}
