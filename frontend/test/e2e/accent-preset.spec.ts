import { expect, test, type Page } from "@playwright/test";

/**
 * The accent preset picker (issue #2493). Modelled on
 * `theme-toggle-visible.spec.ts`'s pin-before-boot pattern, since E1 below has
 * the same shape as that spec's whole reason for existing: what matters is
 * the *first* paint, not the state after an effect has had a chance to run.
 */

/** The first-run tour opens a modal over a fresh console and eats every click. */
test.beforeEach(async ({ page }) => {
  await page.addInitScript(() => {
    const real = Storage.prototype.getItem;
    Storage.prototype.getItem = function getItem(key: string) {
      return key.startsWith("oc-tour:") ? '{"skipped":true}' : real.call(this, key);
    };
  });
});

const ACCENT_KEY = "oc.appearance.accentPreset";

/** Pin both the mode and the accent before the app boots, so the first paint
 *  is the one under test — the same reason `theme-toggle-visible.spec.ts`
 *  pins `"theme"` this way rather than clicking the toggle after load. */
async function pinAndOpenAppearance(page: Page, theme: "dark" | "light", preset?: string) {
  await page.addInitScript(
    ({ theme, preset, key }) => {
      window.localStorage.setItem("theme", theme);
      if (preset) window.localStorage.setItem(key, preset);
    },
    { theme, preset, key: ACCENT_KEY },
  );
  await page.goto("/#/settings/appearance");
  await expect(page.getByRole("radiogroup", { name: "Accent preset" })).toBeVisible();
  await expect(page.locator("html")).toHaveClass(new RegExp(`\\b${theme}\\b`));
}

function radio(page: Page, label: string) {
  return page.getByRole("radio", { name: label });
}

/** `--brand-500`/`--brand-400`, resolved on `<html>` — i.e. whatever the
 *  currently-active preset (or its absence) actually produces. */
async function resolvedBrand500(page: Page) {
  return page.evaluate(() => getComputedStyle(document.documentElement).getPropertyValue("--brand-500").trim());
}

test.describe("no flash from default to the pinned preset (E1)", () => {
  test("the attribute is already correct by DOMContentLoaded — before any effect could run", async ({
    page,
  }) => {
    // `main.tsx` is a blocking `type="module"` script with no `async`/`defer`
    // removed: the browser does not fire `DOMContentLoaded` until it finishes
    // executing top-to-bottom, and `applyStoredAccentPreset()` runs
    // synchronously in that same script, before `mount()`. So by
    // `DOMContentLoaded` the attribute is guaranteed set — no polling, no
    // race, this is just what "runs before mount()" means in a
    // single-threaded script.
    //
    // This is also what catches the regression the test exists for: a plain
    // `useEffect` (the shape `next-themes` itself uses for `.dark`,
    // `roadblocks.md` R1) is scheduled as a passive effect and does not run
    // synchronously inside the script that produces `DOMContentLoaded` — so
    // if `applyStoredAccentPreset()` ever moved into one, this check would
    // capture the attribute *before* that effect had a chance to run.
    await page.addInitScript(() => {
      (window as unknown as { __oc_e1?: string }).__oc_e1 = "__not_yet_checked__";
      document.addEventListener(
        "DOMContentLoaded",
        () => {
          (window as unknown as { __oc_e1?: string }).__oc_e1 =
            document.documentElement.dataset.accentPreset ?? "__absent__";
        },
        { once: true },
      );
    });
    await pinAndOpenAppearance(page, "light", "rose");
    expect(await page.evaluate(() => (window as unknown as { __oc_e1?: string }).__oc_e1)).toBe("rose");
  });
});

test.describe("resolved colours (E2)", () => {
  for (const theme of ["light", "dark"] as const) {
    test(`switching to Teal changes --brand-500, in ${theme} mode`, async ({ page }) => {
      await pinAndOpenAppearance(page, theme);
      const before = await resolvedBrand500(page);
      await radio(page, "Teal").click();
      await expect(page.locator("html")).toHaveAttribute("data-accent-preset", "teal");
      const after = await resolvedBrand500(page);
      expect(after).not.toBe(before);
    });
  }

  test("a chart-1 mark does not move when the preset changes", async ({ page }) => {
    await pinAndOpenAppearance(page, "light");
    const before = await page.evaluate(() =>
      getComputedStyle(document.documentElement).getPropertyValue("--chart-1").trim(),
    );
    await radio(page, "Green").click();
    const after = await page.evaluate(() =>
      getComputedStyle(document.documentElement).getPropertyValue("--chart-1").trim(),
    );
    expect(after).toBe(before);
  });
});

test.describe("picker round trip (E3)", () => {
  test("choosing a preset persists across reload, and the Theme control keeps working", async ({
    page,
  }) => {
    await pinAndOpenAppearance(page, "light");
    await radio(page, "Indigo").click();
    await expect(page.locator("html")).toHaveAttribute("data-accent-preset", "indigo");
    await expect.poll(() => page.evaluate((k) => window.localStorage.getItem(k), ACCENT_KEY)).toBe(
      "indigo",
    );

    await page.reload();
    await expect(page.locator("html")).toHaveAttribute("data-accent-preset", "indigo");

    // The two controls are independent axes — switching mode must not disturb
    // the persisted preset, and the preset radiogroup existing must not have
    // broken the theme dropdown it sits beside.
    await page.getByRole("button", { name: "Change theme" }).click();
    await page.getByRole("menuitem", { name: "Dark" }).click();
    await expect(page.locator("html")).toHaveClass(/\bdark\b/);
    await expect(page.locator("html")).toHaveAttribute("data-accent-preset", "indigo");

    await radio(page, "Default").click();
    await expect(page.locator("html")).not.toHaveAttribute("data-accent-preset", /.+/);
    await expect.poll(() => page.evaluate((k) => window.localStorage.getItem(k), ACCENT_KEY)).toBeNull();
  });
});

test.describe("a swatch shows its own colour (E4)", () => {
  test("with Rose active, the Teal swatch still paints teal — not rose", async ({ page }) => {
    await pinAndOpenAppearance(page, "light", "rose");

    const tealSwatchBg = await radio(page, "Teal")
      .locator("span")
      .first()
      .evaluate((el) => getComputedStyle(el).backgroundColor);

    // A ground-truth `bg-brand-500` element, scoped to `data-accent-preset="teal"`
    // and appended into the real document, resolved in computed rgb() the same
    // way the swatch's `backgroundColor` above was. If the swatch used
    // `bg-primary` instead (`roadblocks.md` R4), it would resolve against the
    // page's active preset — rose — and this equality would fail.
    const tealReferenceBg = await page.evaluate(() => {
      const probe = document.createElement("div");
      probe.dataset.accentPreset = "teal";
      probe.className = "bg-brand-500";
      document.body.appendChild(probe);
      const value = getComputedStyle(probe).backgroundColor;
      probe.remove();
      return value;
    });

    expect(tealSwatchBg).toBe(tealReferenceBg);

    // And a negative control: the swatch's colour must actually differ from
    // the page's own active (rose) --brand-500, or the equality above would
    // be true for a trivial reason (a picker with only one colour, say).
    const roseBrand500 = await resolvedBrand500(page);
    const tealBrand500 = await page.evaluate(() => {
      const probe = document.createElement("div");
      probe.dataset.accentPreset = "teal";
      document.body.appendChild(probe);
      const value = getComputedStyle(probe).getPropertyValue("--brand-500").trim();
      probe.remove();
      return value;
    });
    expect(tealBrand500).not.toBe(roseBrand500);
  });
});
