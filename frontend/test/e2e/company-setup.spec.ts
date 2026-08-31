import { expect, test, type APIRequestContext, type Page } from "@playwright/test";

import { FIRST_RUN_COMPANY } from "./capabilities";

/**
 * First-run company setup, end to end
 * (`docs/spec/runtime/company-setup.md`).
 *
 * Three questions asked once, then a team created on the host. This spec covers
 * the half only a browser proves: that the dialog opens by itself on a company
 * nobody has staffed, that the build-out screen names each teammate as its write
 * lands, and that the roster the operator is left looking at came from the
 * **host** rather than from the console's fabricated starter team.
 *
 * A unit test pins the decisions (`test/unit/company-setup.test.ts`); this pins
 * that they are wired to a real host.
 *
 * # This lane needs its own company, and now has its own run
 *
 * Setup opens only on a company nobody has staffed, and every company under
 * `companies/` except one declares agents of its own — including
 * `companies/e2e_harness`, which the rest of the suite drives. So this spec
 * needs a different host, which is why it is a separate run:
 *
 * ```sh
 * npm run e2e:first-run
 * ```
 *
 * That sets `PW_FIRST_RUN=1`, and `playwright.config.ts` does the rest: it
 * serves `companies/e2e_setup` on a data root of its own, and selects this spec
 * and only this spec. An ordinary `npx playwright test` does not run it at all —
 * not by skipping it, by not selecting it.
 *
 * # Why the guard below fails instead of skipping (issue #1404)
 *
 * What stood here was:
 *
 * ```ts
 * test.skip(left.length > 0, "this company ships with N manifest agents ...");
 * ```
 *
 * It was written for a host serving the wrong company. What it actually did,
 * once the global baseline began merging four teammates of its own into
 * **every** company, was fire on every run — including the right one. So the
 * lane went green while first-run setup could not open anywhere in the shipped
 * product, and nothing said a word. That is `CLAUDE.md`'s "builds, runs and
 * reports zero without failing anything", one level up from the Rust targets it
 * describes.
 *
 * A first-run lane that skips itself is worse than no lane, so the guard is now
 * an assertion. A run pointed at the wrong host fails on its first line, naming
 * the command to use, rather than passing vacuously.
 */

const COMPANY_SCOPE = "/api/v1/company";

/** One row of the host's roster, as this spec reads it. */
type RosterRow = { id?: string; role: string; global?: boolean };

/** The roster the host actually holds — baseline teammates included. */
async function hostRoster(request: APIRequestContext): Promise<RosterRow[]> {
  const res = await request.get(`${COMPANY_SCOPE}/team`);
  expect(res.ok()).toBeTruthy();
  return (await res.json()) as RosterRow[];
}

/**
 * The teammates somebody staffed this company with — the roster minus the global
 * baseline (`docs/spec/runtime/globals.md`), which is merged into every company
 * and is therefore evidence of nothing.
 *
 * This is the quantity the whole spec is about. `hostRoster().length` is never
 * zero on any company this host can serve, which is exactly the confusion
 * issue #1404 was filed over; asserting on it would put the bug back.
 */
function staffed(roster: RosterRow[]): RosterRow[] {
  return roster.filter((member) => member.global !== true);
}

/**
 * Where one teammate came from: `"manifest"` for a row the company declares in
 * `company.toml` (the global baseline is merged in as manifest rows too),
 * `"overlay"` for one somebody added through the console.
 *
 * The roster read does not carry it — only the detail read does — and the
 * distinction is what keeps the guard below honest, so it is worth the extra
 * request per staffed row.
 *
 * It fails **closed**, which is the whole reason it is a separate function. A
 * manifest teammate is deletable now, so guessing `"overlay"` on a detail read
 * that failed or answered without a `source` would tombstone a blueprint
 * teammate on a host this lane was never meant to touch — and `unstaffCompany`
 * discards DELETE errors, so nothing would say a word. An unreadable source is
 * not a deletable one: throw, naming the id, and let the run fail on its own
 * terms.
 */
async function sourceOf(request: APIRequestContext, id: string): Promise<string> {
  const res = await request.get(`${COMPANY_SCOPE}/team/${id}`);
  expect(res.ok(), `could not read teammate '${id}' to find out where it came from`).toBeTruthy();
  const source = ((await res.json()) as { source?: string }).source;
  expect(
    source,
    `the host did not say where teammate '${id}' came from, so this helper cannot tell a ` +
      "blueprint teammate it must leave alone from an operator-added one it may remove",
  ).toBeDefined();
  return source as string;
}

/**
 * Removes every operator-added teammate, so a re-run starts from a first run
 * again — and refuses to touch a host that is not this lane's company.
 *
 * This used to fire a DELETE at every row and lean on the host to sort them
 * out: a manifest or baseline teammate answered `409` and was left where it
 * was, which both unstaffed the company and protected a host serving the wrong
 * one. Neither holds any more. A manifest teammate is removable now, so the
 * blanket loop would *wipe* the roster of a host running the rest of the suite;
 * and the roster's one remaining refusal is its **last** teammate, so the
 * blanket loop leaves exactly one survivor — the staffed row the guard is
 * looking for, since the baseline is listed first. That is the leftover
 * `Accountant` this lane failed on.
 *
 * So the sorting happens here instead. Manifest rows are identified and left
 * strictly alone, which keeps the wrong-host assertion below meaningful (#1404)
 * rather than something this helper has already deleted its way past; only the
 * rows the host calls `"overlay"` are removed, and because the baseline stays
 * the last-teammate refusal is never reached and every delete lands.
 */
async function unstaffCompany(request: APIRequestContext) {
  for (const member of staffed(await hostRoster(request))) {
    if (!member.id) continue;
    // Deleted only on an explicit `"overlay"`. Anything else — a blueprint row,
    // or a source this spec does not know — is left where it is.
    if ((await sourceOf(request, member.id)) !== "overlay") continue;
    await request.delete(`${COMPANY_SCOPE}/team/${member.id}`).catch(() => undefined);
  }
}

/** Answers one question and advances. */
async function answer(page: Page, field: string, text: string) {
  await expect(page.getByTestId(`setup-field-${field}`)).toBeVisible();
  await page.getByTestId(`setup-field-${field}`).fill(text);
  await page.getByTestId("setup-next").click();
}

test.beforeEach(async ({ request }) => {
  await unstaffCompany(request);
  // Anyone still here after unstaffing is declared in the manifest, which
  // `unstaffCompany` deliberately does not remove, so setup can never open on
  // this host. Fail now, naming the command that fixes it — a skip here is what
  // made this whole spec vacuous (#1404).
  const left = staffed(await hostRoster(request));
  expect(
    left.map((member) => member.role),
    `this host serves a company that ships with ${left.length} teammate(s) of its ` +
      "own, so first-run setup cannot open against it. Run this spec with " +
      `\`npm run e2e:first-run\`, which serves ${FIRST_RUN_COMPANY}.`,
  ).toEqual([]);
});

test("first-run setup builds a real team from three answers", async ({ page, request }) => {
  await page.addInitScript(() => {
    // Clear any skip recorded by an earlier run in this browser profile, and the
    // tour's own seen flag, so neither suppresses what this spec is watching.
    for (const key of Object.keys(window.localStorage)) {
      if (key.startsWith("oc-setup") || key.startsWith("oc-tour")) {
        window.localStorage.removeItem(key);
      }
    }
  });

  await page.goto("/#/overview");

  // 1. It opens by itself — nobody clicked anything.
  const dialog = page.getByTestId("setup-dialog");
  await expect(dialog).toBeVisible({ timeout: 20_000 });
  await expect(page.getByTestId("setup-question")).toContainText("What kind of company");
  // This host has no model. Say what that changes before collecting answers,
  // rather than after presenting a plausible but standard roster.
  const modelNotice = page.getByTestId("setup-inference-notice");
  await expect(modelNotice).toContainText("can't design your team with a model");
  // A harness-less binary can never put a model on the design path, so the
  // "Set up a model" CTA is a dead end here and is rightly omitted.
  await expect(modelNotice.getByRole("link", { name: "Set up a model" })).toHaveCount(0);

  // The tour must be holding: a walkthrough of an unstaffed company is the
  // first impression this feature exists to replace.
  await expect(page.getByRole("button", { name: "Take the tour" })).toBeHidden();

  // 2. The first question is required; the other two are not.
  await page.getByTestId("setup-next").click();
  await expect(page.getByTestId("setup-problem")).toBeVisible();

  await answer(page, "industry", "E-commerce — I sell homeware online");
  await answer(page, "teamHint", "");
  await answer(page, "automate", "Meta ads, order dispatch, daily sales reports");

  // 3. The build-out names each teammate as its write lands.
  await expect(page.getByTestId("setup-buildout-title")).toBeVisible({ timeout: 60_000 });
  const created = page.getByTestId("setup-agent-created");
  await expect(created.first()).toBeVisible({ timeout: 30_000 });

  // 4. It finishes, and says so as a starting point rather than a fait accompli.
  await expect(page.getByTestId("setup-buildout-title")).toContainText("standard team", {
    timeout: 60_000,
  });
  // Same no-CTA rule on completion: this binary cannot run the design pass, so
  // "Add a model in Settings" would send the operator round a loop that cannot
  // end — there is no model setting that helps.
  await expect(page.getByTestId("setup-add-model")).toHaveCount(0);
  const names = await created.allInnerTexts();
  expect(names.length, `build-out listed ${names.length} agents`).toBeGreaterThanOrEqual(4);

  await page.getByTestId("setup-finish").click();
  await expect(dialog).toBeHidden();
  await expect(page).toHaveURL(/#\/company$/);

  // Setup's payoff is the roster, and its own build-out is the introduction;
  // the first-run welcome must not immediately cover either one.
  await expect(page.getByRole("button", { name: "Take the tour" })).toBeHidden();

  // 5. The host really holds them — not the console's fabricated starter team,
  //    and not the global baseline every company already had.
  const designed = staffed(await hostRoster(request));
  expect(
    designed.length,
    "the teammates setup created, over and above the baseline every company gets",
  ).toBeGreaterThanOrEqual(4);

  // 6. The arrival page shows that roster, refreshed without a reload.
  for (const member of designed.slice(0, 3)) {
    await expect(page.getByText(member.role, { exact: false }).first()).toBeVisible();
  }

  // 7. A reload does not re-offer setup: the roster is no longer empty, which is
  // the whole reason emptiness is the signal rather than a stored flag.
  await page.reload();
  await page.goto("/#/overview");
  await expect(dialog).toBeHidden();
});

test("skipping setup leaves a way back in", async ({ page, request }) => {
  await page.addInitScript(() => {
    for (const key of Object.keys(window.localStorage)) {
      if (key.startsWith("oc-setup") || key.startsWith("oc-tour")) {
        window.localStorage.removeItem(key);
      }
    }
  });

  await page.goto("/#/overview");
  await expect(page.getByTestId("setup-dialog")).toBeVisible({ timeout: 20_000 });

  await page.getByTestId("setup-skip").click();
  await expect(page.getByTestId("setup-dialog")).toBeHidden();

  // Skipping must not be a dead end: the Team page keeps offering it in place.
  await page.goto("/#/company");
  await expect(page.getByTestId("setup-prompt")).toBeVisible({ timeout: 20_000 });

  // And nothing was created by skipping. The baseline is still there — it always
  // is — so this asks the only question that distinguishes the two states.
  expect(staffed(await hostRoster(request))).toHaveLength(0);

  // The prompt reopens the same dialog.
  await page.getByTestId("setup-prompt-run").click();
  await expect(page.getByTestId("setup-dialog")).toBeVisible();
});
