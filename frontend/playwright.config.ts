import { defineConfig } from '@playwright/test';

/**
 * Playwright config for the operator-console end-to-end suite.
 *
 * The suite drives a *running* OpenCompany host (the Rust binary serving the
 * built `frontend/dist` via `OPENCOMPANY_CONSOLE_DIR`) at `PW_BASE_URL`,
 * defaulting to the binary's own default bind. Bringing that host up — with a
 * mocked LLM backend — is the harness's job, not this config's, so no
 * `webServer` is declared here.
 */
export default defineConfig({
  testDir: './test/e2e',
  fullyParallel: false,
  workers: 1,
  timeout: 60_000,
  reporter: [['list']],
  use: {
    baseURL: process.env.PW_BASE_URL || 'http://127.0.0.1:8080',
    trace: 'on-first-retry',
    screenshot: 'only-on-failure',
  },
});
