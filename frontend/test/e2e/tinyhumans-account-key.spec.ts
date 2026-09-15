import { expect, test } from "@playwright/test";

/**
 * The TinyHumans account key, end to end, against a **real** TinyHumans
 * backend (keys rework #2306, decision X10's "cannot be exercised here"
 * closed by the umbrella repo).
 *
 * `account-key-fanout.spec.ts` stubs every `…/credential` round trip because
 * this repo has no TinyHumans key to give the host. This spec is the other
 * half: the umbrella's `scripts/e2e-tinyhumans-key.sh` stands up the backend
 * in `MOCK_LLM` mode, mints an account key there, starts this host with
 * `TINYHUMANS_API_URL` pointed at it and **no** inference credential in the
 * environment, and hands the key in through `PW_TINYHUMANS_KEY`. Nothing is
 * stubbed: the save fans out to the LLM `tinyhumans` row, the health probe
 * lists the backend's OpenRouter proxy catalog, the model picked here becomes
 * the company default, the runtime is rebuilt onto the harness, and the chat
 * turn that follows is answered by the backend's mocked completion through
 * `/agent-integrations/openrouter/chat/completions`.
 *
 * What it proves, in order:
 *  1. a host that booted on the echo brain (no key anywhere) accepts the
 *     account key and asks for a model (`needsModel`);
 *  2. the model step lists what the backend's proxy catalog returned;
 *  3. the save reports no restart — the host rebuilt the company in place;
 *  4. the LLM page shows `tinyhumans` as `Default · <model>`;
 *  5. a chat turn is answered by the backend (`__MOCK_LLM__`), not `You said:`.
 *
 * Skipped without `PW_TINYHUMANS_KEY`: there is no honest way to run it
 * against a host that has no backend behind it.
 */

const KEY = process.env.PW_TINYHUMANS_KEY ?? "";
const MODEL = process.env.PW_TINYHUMANS_MODEL ?? "";

test.skip(
  !KEY || !MODEL,
  "PW_TINYHUMANS_KEY and PW_TINYHUMANS_MODEL name the backend-minted account key and a " +
    "model its OpenRouter proxy lists; the umbrella's scripts/e2e-tinyhumans-key.sh sets both.",
);

// The first-run product tour opens a Radix dialog over the console; see
// `wiring.spec.ts` for why every spec that clicks anything skips it this way.
test.beforeEach(async ({ page }) => {
  await page.addInitScript(() => {
    const real = Storage.prototype.getItem;
    Storage.prototype.getItem = function getItem(key: string) {
      return key.startsWith("oc-tour:") ? '{"skipped":true}' : real.call(this, key);
    };
  });
});

type Page = import("@playwright/test").Page;

const toasts = (page: Page) => page.locator("[data-sonner-toast]");

/**
 * Clears the activation gate ("Skip setup") and the tour ("Skip for now") if
 * either is up, then re-opens `hash` — dismissing the gate navigates away
 * from a deep link. The umbrella runner starts the host with
 * `OPENCOMPANY_SKIP_ACTIVATION_GATE=1`, so normally neither appears; this is
 * for a hand-started host. Same shape as `agent-session.spec.ts`'s helper.
 */
async function open(page: Page, hash: string): Promise<void> {
  await page.goto(hash);
  const any = page.getByRole("button", { name: /^(Skip setup|Skip for now)$/ });
  await any
    .first()
    .waitFor({ state: "visible", timeout: 3_000 })
    .catch(() => {});
  let dismissed = false;
  for (const name of ["Skip setup", "Skip for now"]) {
    const skip = page.getByRole("button", { name });
    for (let attempt = 0; attempt < 5; attempt += 1) {
      if (!(await skip.isVisible().catch(() => false))) break;
      dismissed = true;
      await skip.click({ force: true }).catch(() => {});
      await page.waitForTimeout(300);
    }
  }
  if (dismissed) await page.goto(hash);
}

test("the account key sets up TinyHumans for LLM and a turn reaches the backend", async ({
  page,
}) => {
  // Precondition: the host booted with no inference source, so the turn at the
  // end is only answered by the backend if THIS flow wired it. A host that
  // already thinks would make the last assertion prove nothing.
  const before = await page.request.get("/api/v1/company/inference");
  expect(before.ok()).toBeTruthy();
  const beforeStatus = (await before.json()) as { cognition: string; defaultChoice?: unknown };
  expect(beforeStatus.cognition, "the host must start on the echo brain").toBe("echo");

  // 1. Account page → add the key.
  await open(page, "/#/connections/api-key");
  await expect(
    page.getByTestId("account-rows").or(page.getByTestId("account-empty")),
  ).toBeVisible({ timeout: 30_000 });
  await page.getByTestId("account-add-key").click();
  await page.getByTestId("account-key-input").fill(KEY);
  await page.getByTestId("account-key-save").click();

  // 2. The host probed the backend, listed its proxy catalog, and asks for a
  //    model. The picker's options ARE the backend's answer.
  const modelStep = page.getByTestId("account-key-model-step");
  await expect(modelStep).toBeVisible({ timeout: 30_000 });
  await modelStep.locator("#account-key-model").click();
  const option = page.getByRole("option", { name: new RegExp(`^${escapeRegExp(MODEL)}`) });
  await expect(option).toBeVisible({ timeout: 10_000 });
  await option.click();
  await page.getByTestId("account-key-model-save").click();

  // 3. Saved, and live: the host rebuilt the company rather than asking for a
  //    restart. A "restart required" headline here is the bug this spec was
  //    written against.
  const saved = toasts(page).filter({ hasText: /Key saved/ }).first();
  await expect(saved).toBeVisible({ timeout: 30_000 });
  await expect(saved).not.toContainText("restart required");
  await expect(saved).toContainText(`TinyHumans is set up for LLM with ${MODEL}`);

  const after = await page.request.get("/api/v1/company/inference");
  const afterStatus = (await after.json()) as {
    cognition: string;
    restartRequired: boolean;
    defaultChoice: { provider: string; model: string; broken: boolean } | null;
    providers: { slug: string; baseUrl: string; isDefault: boolean; enabled: boolean }[];
  };
  expect(afterStatus.cognition).toBe("harness");
  expect(afterStatus.restartRequired).toBe(false);
  expect(afterStatus.defaultChoice).toEqual({ provider: "tinyhumans", model: MODEL, broken: false });
  const row = afterStatus.providers.find((p) => p.slug === "tinyhumans");
  expect(row, "the fan-out created the tinyhumans row").toBeTruthy();
  expect(row?.enabled).toBe(true);
  // The row follows `TINYHUMANS_API_URL`, never the production host — that is
  // what lets a staging or local platform mint a key the host can use.
  expect(row?.baseUrl).toMatch(/\/agent-integrations\/openrouter$/);
  expect(row?.baseUrl).not.toContain("api.tinyhumans.ai");

  // 4. The LLM page agrees, from its own read.
  await open(page, "/#/connections/inference");
  await expect(page.getByTestId("inference-provider-tinyhumans")).toBeVisible({
    timeout: 30_000,
  });
  await expect(page.getByTestId("inference-provider-tinyhumans-default")).toContainText(
    `Default · ${MODEL}`,
  );
  await expect(page.getByTestId("inference-restart-required")).toHaveCount(0);

  // 5. A turn goes through the backend. On the reply's prose rather than the
  //    `__MOCK_LLM__` marker: Room renders the line as Markdown, and the
  //    marker's underscores become emphasis (see `wiring.spec.ts`).
  await open(page, "/#/chat");
  const prompt = `tinyhumans account key e2e ${Date.now()}`;
  await page.getByPlaceholder(/^Message /).fill(prompt);
  await page.getByRole("button", { name: "Send", exact: true }).click();
  await expect(page.getByText(/MOCK_LLM/).first()).toBeVisible({ timeout: 90_000 });
  await expect(page.getByText(/^You said:/)).toHaveCount(0);
  await expect(page.getByText(/^Couldn't send/)).toHaveCount(0);
});

function escapeRegExp(value: string): string {
  return value.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}
