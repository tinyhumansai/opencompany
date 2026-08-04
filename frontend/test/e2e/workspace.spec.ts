import { expect, test, type APIRequestContext, type Page } from "@playwright/test";

/**
 * Regression proof for issue #177: the Workspace tab must read and write the
 * **host's** `WorkspaceStore`, never a client-side localStorage tree.
 *
 * The retired stub (`lib/workspace.ts`) persisted to `oc-workspace:<company>`
 * and seeded four invented marketing notes — "Spring launch", "Brand voice" —
 * into every browser. So the operator's workspace and the tree the company's
 * agents read and write through their `workspace_*` tools (#237) were two
 * unrelated things: a note an agent wrote was invisible in the console, and a
 * note the operator typed was invisible to every agent.
 *
 * This spec pins the four observable consequences of the fix:
 *
 *   1. The tree the tab renders is the one the host serves — including the
 *      company's seeded notes, which no client-side fixture ever had.
 *   2. Notes created in the tab survive a *storage-cleared* reload. Only host
 *      state can survive that; a localStorage tree could not.
 *   3. A write made **out of band** over REST (standing in for an agent, or a
 *      second browser) shows up after a refresh.
 *   4. Editing in the tab persists — the debounced autosave reaches the host.
 *
 * Plus: none of the retired fixture's invented notes ever render.
 *
 * Runs against the same live host as `wiring.spec.ts` (see that file's header
 * for the auth / storage-state arrangement).
 */

/** Notes the deleted client-side seed fabricated for every browser. */
const FIXTURE_NOTES = ["Spring launch", "Brand voice", "Campaigns"];

/**
 * A fresh host greets the first visit with a welcome tour rendered over the
 * console, which swallows clicks on the view beneath it. Dismiss it if present.
 */
async function dismissOnboarding(page: Page) {
  const skip = page.getByRole("button", { name: "Skip for now" });
  await skip.waitFor({ state: "visible", timeout: 15_000 }).catch(() => {});
  if (await skip.isVisible()) {
    await skip.click();
    await expect(skip).toBeHidden();
  }
}

/**
 * Open the Workspace tab on a genuinely fresh app instance.
 *
 * The explicit `reload()` is load-bearing: the console is a hash router, so a
 * second `goto("/#/workspace")` at the same URL does not remount anything and
 * the view keeps whatever it already had in React state. Without the reload,
 * "survives a storage-cleared reload" would assert nothing, and the migration
 * effect (which runs on mount) would never re-run.
 */
async function openWorkspace(page: Page) {
  await page.goto("/#/workspace");
  await page.reload();
  await dismissOnboarding(page);
  await expect(page.getByTestId("workspace-tree")).toBeVisible({ timeout: 30_000 });
}

/**
 * The localStorage key the retired scratchpad used for this host, derived the
 * same way the app derives the company it operates: the console resolves a lone
 * registered company to its **id** (`App.tsx`), falling back to `single` when
 * there is none. Hardcoding `oc-workspace:single` silently plants a key nothing
 * reads, and the migration assertions below would then pass vacuously.
 */
async function legacyKeyFor(request: APIRequestContext): Promise<string> {
  const companies = (await (await request.get("/api/v1/companies")).json()) as { id: string }[];
  return `oc-workspace:${companies.length === 1 ? companies[0].id : "single"}`;
}

test("the Workspace tab reads and writes the host's store, not localStorage", async ({
  page,
  request,
}) => {
  // A unique name per run, so re-running the suite against a persistent host
  // never collides with a previous run's note.
  const stamp = Date.now().toString(36);
  const viaApi = `api-note-${stamp}`;
  const viaUi = `ui-note-${stamp}`;

  // ---- 1. The tab renders the host's tree -------------------------------
  const tree = (await (await request.get("/api/v1/company/workspace")).json()) as {
    id: string;
    name: string;
    kind: string;
  }[];
  await openWorkspace(page);

  // The harness company seeds `README.md` + `Standards.md`, so a real seeded
  // note must be on screen. It came from the company bundle, not the console.
  const seeded = tree.find((n) => n.name === "README.md");
  expect(seeded, "the harness company must seed a workspace").toBeTruthy();
  await expect(page.getByTestId("workspace-tree")).toContainText("README", {
    timeout: 30_000,
  });

  // None of the retired fixture's invented notes survive anywhere.
  for (const invented of FIXTURE_NOTES) {
    await expect(page.getByText(invented, { exact: false })).toHaveCount(0);
  }

  // ---- 2. A note written out of band appears on refresh -----------------
  // This is the half that was structurally impossible before: an agent writing
  // through `workspace_write` lands in exactly this store.
  const created = await request.post("/api/v1/company/workspace", {
    data: { name: `${viaApi}.md`, kind: "file", content: `# ${viaApi}\n\nWritten over REST.` },
  });
  expect(created.ok()).toBeTruthy();

  await page.getByTestId("workspace-refresh").click();
  await expect(page.getByTestId("workspace-tree")).toContainText(viaApi, { timeout: 30_000 });

  // Opening it shows the body the host holds — the tree carries no content, so
  // this proves the per-file read is wired too.
  await page.getByTestId("workspace-tree").getByText(viaApi, { exact: false }).click();
  await expect(page.getByTestId("workspace-note")).toContainText("Written over REST", {
    timeout: 30_000,
  });

  // ---- 3. A note created in the tab reaches the host --------------------
  await page.getByRole("button", { name: "New file" }).click();
  await page.getByLabel("Name").fill(viaUi);
  await page.getByRole("button", { name: "Create", exact: true }).click();

  // Creating opens the new note straight in Edit; type and let autosave settle.
  const editor = page.getByTestId("workspace-editor");
  await expect(editor).toBeVisible({ timeout: 30_000 });
  await editor.fill(`# ${viaUi}\n\nTyped in the console.`);
  await expect(page.getByTestId("workspace-save-state")).toHaveAttribute("data-state", "saved", {
    timeout: 30_000,
  });

  // The host has it — asserted over the API, not by reading the UI back.
  const after = (await (await request.get("/api/v1/company/workspace")).json()) as {
    id: string;
    name: string;
  }[];
  const persisted = after.find((n) => n.name === `${viaUi}.md`);
  expect(persisted, "a note created in the console must reach the host").toBeTruthy();
  const file = (await (
    await request.get(`/api/v1/company/workspace/file/${persisted!.id}`)
  ).json()) as { content: string };
  expect(file.content).toContain("Typed in the console");

  // ---- 4. It survives a storage-cleared reload --------------------------
  // Drop every client-side store, then reload: only host state can survive.
  await page.evaluate(() => {
    localStorage.clear();
    sessionStorage.clear();
  });
  await openWorkspace(page);
  await expect(page.getByTestId("workspace-tree")).toContainText(viaUi, { timeout: 30_000 });
  await expect(page.getByTestId("workspace-tree")).toContainText(viaApi, { timeout: 30_000 });

  // ---- cleanup: leave the host as we found it ---------------------------
  for (const name of [`${viaApi}.md`, `${viaUi}.md`]) {
    const node = after.find((n) => n.name === name) ?? tree.find((n) => n.name === name);
    if (node) await request.delete(`/api/v1/company/workspace/${node.id}`);
  }
});

/**
 * The localStorage migration: notes a user typed into the retired scratchpad are
 * offered for import rather than silently dropped — but the *bundled seed* is
 * swept without asking, because importing it would inject invented marketing
 * copy into a real company's workspace.
 */
test("the retired local scratchpad is offered for import, its seed discarded", async ({
  page,
  request,
}) => {
  const stamp = Date.now().toString(36);
  const rescued = `local-note-${stamp}`;
  const legacyKey = await legacyKeyFor(request);

  // Plant a legacy key holding one user-authored note (`fs-…`) alongside a
  // bundled seed node (`seed-…`), exactly as the retired store would have.
  await page.goto("/#/workspace");
  await dismissOnboarding(page);
  await page.evaluate(
    ([key, name]) => {
      localStorage.setItem(
        key,
        JSON.stringify([
          {
            id: "seed-readme",
            name: "README.md",
            kind: "file",
            parentId: null,
            updatedAt: Date.now(),
            content: "# Workspace\n\n[[Spring launch]]",
          },
          {
            id: "fs-user-1",
            name: `${name}.md`,
            kind: "file",
            parentId: null,
            updatedAt: Date.now(),
            content: "Something the operator actually typed.",
          },
        ]),
      );
    },
    [legacyKey, rescued] as const,
  );

  await openWorkspace(page);

  // The banner offers only the user's note — the seed is not counted.
  const banner = page.getByTestId("workspace-migration-banner");
  await expect(banner).toBeVisible({ timeout: 30_000 });
  await expect(banner).toContainText("1 note");

  await page.getByTestId("workspace-migration-import").click();
  await expect(banner).toBeHidden({ timeout: 30_000 });

  // The rescued note is on the host, under the import folder.
  const nodes = (await (await request.get("/api/v1/company/workspace")).json()) as {
    id: string;
    name: string;
    kind: string;
    parentId?: string | null;
  }[];
  const folder = nodes.find((n) => n.kind === "folder" && n.name === "Imported from this browser");
  expect(folder, "the import must land in its own folder").toBeTruthy();
  const note = nodes.find((n) => n.name === `${rescued}.md`);
  expect(note, "the user's note must be imported").toBeTruthy();
  expect(note!.parentId).toBe(folder!.id);

  // The seed's invented note was never imported.
  expect(nodes.some((n) => n.name.includes("Spring launch"))).toBe(false);

  // The legacy key is gone, so the banner does not come back.
  const leftover = await page.evaluate((key) => localStorage.getItem(key), legacyKey);
  expect(leftover).toBeNull();

  // ---- cleanup ----
  if (folder) await request.delete(`/api/v1/company/workspace/${folder.id}`);
});
