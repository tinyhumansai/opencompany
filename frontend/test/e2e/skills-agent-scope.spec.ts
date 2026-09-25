import { randomUUID } from "node:crypto";

import { expect, test, type APIRequestContext, type Page } from "@playwright/test";

/** The company's effective skills, as the host serves them. */
interface HostSkill {
  id: string;
  name: string;
  description: string;
  category: string;
  source: string;
  enabled: boolean;
  version?: string | null;
  updatedAtMillis?: number | null;
}

/** The company's effective skill set, read straight from the host. */
async function hostSkills(request: APIRequestContext): Promise<HostSkill[]> {
  const answer = await request.get("/api/v1/company/skills");
  expect(answer.ok(), `GET …/skills failed: ${answer.status()}`).toBeTruthy();
  return (await answer.json()) as HostSkill[];
}

/**
 * Invites `email` as a member and redeems a login code into `context`.
 *
 * Lifted from `settings-authority.spec.ts`, which is where the suite's
 * second-principal pattern already lives: the shared storage state is the
 * harness admin, and a member signs in through the same magic-link flow the
 * product uses, in a browser context of its own. A re-run hits `409 already a
 * member`, which is a success here — the address can sign in either way.
 */
async function signInAsMember(
  admin: APIRequestContext,
  context: APIRequestContext,
  email: string,
) {
  const invited = await admin.post("/api/v1/company/users/invites", {
    data: { email, role: "member" },
  });
  expect(
    invited.ok() || invited.status() === 409,
    `inviting ${email} failed: ${invited.status()} ${await invited.text()}`,
  ).toBeTruthy();

  const requested = await context.post("/api/v1/company/auth/request", {
    data: { email },
  });
  const devCode = (await requested.json())?.dev_code as string | undefined;
  expect(
    devCode,
    "no dev_code came back — the host must bind loopback with no mail transport " +
      "configured for the member half of this spec to sign in",
  ).toBeTruthy();

  const verified = await context.post("/api/v1/company/auth/verify", {
    data: { code: devCode },
  });
  expect(verified.ok(), `member sign-in failed: ${await verified.text()}`).toBeTruthy();
  expect((await verified.json()).role).toBe("member");
}

/**
 * Suppresses the first-run tour before the app boots.
 *
 * The tour renders over the console and swallows clicks on the view beneath
 * it. Seeding its own markers through an init script means it never paints, so
 * there is nothing to wait for — the pattern `agent-detail.spec.ts` uses, and
 * for its reason: waiting on a dismiss button that is usually absent spends the
 * full timeout on the common case.
 */
async function suppressTour(page: Page) {
  await page.addInitScript(() => {
    const seen = JSON.stringify({ skipped: true, seenAt: Date.now() });
    for (const key of ["oc-tour:single", "oc-tour:e2e-harness-co", "oc-tour:null"]) {
      window.localStorage.setItem(key, seen);
    }
  });
}


/**
 * A teammate's skill scope, from its own page (#2458).
 *
 * Three states, the same shape the tool grant uses: inheriting puts every
 * skill the company has enabled in front of the teammate, an explicit empty
 * list gives it none, and a list narrows. The card's hard part is the first
 * one — an inherited scope renders every switch **on** while storing nothing,
 * so the list a narrowing starts from has to be the company's enabled set
 * rather than the stored one. Get that wrong and the operator's first flick,
 * which reads on screen as "all but this one", writes "only the one I
 * flicked".
 *
 * The spec drives a teammate it creates and then deletes, rather than one of
 * the harness company's blueprint agents. That is not only hygiene: on this
 * branch the reset (`PATCH …/team/{id}` with `{"skills": null}`) does not land
 * for a **manifest** teammate — the override carrying it is dropped on the way
 * to the record — so a spec that narrowed `engineer` would have no way to put
 * it back and would leave every later spec running against a teammate it
 * silently re-scoped. A console-created teammate stores its scope on the
 * overlay directly, where all three states round-trip.
 *
 * Default features are enough: the routes this exercises ship in the default
 * build.
 */

const AGENT_NAME = `Scope Probe ${randomUUID()}`;
/** The id the host mints for `AGENT_NAME`, captured once it exists. */
let AGENT_ID = "";
const MEMBER_EMAIL = "member-skill-scope@example.test";

async function removeAgent(request: APIRequestContext) {
  if (!AGENT_ID) return;
  await request.delete(`/api/v1/company/team/${AGENT_ID}`).catch(() => undefined);
}

/** The company's enabled skills — the set an inherited scope resolves to. */
async function enabledSlugs(request: APIRequestContext): Promise<string[]> {
  return (await hostSkills(request)).filter((skill) => skill.enabled).map((skill) => skill.id);
}

/** Opens the teammate's Tools tab, where the Skills card lives. */
async function openScopeCard(page: Page) {
  await page.goto(`/#/company/agent/${AGENT_ID}`);
  await expect(page.getByTestId("agent-name")).toHaveText(AGENT_NAME, { timeout: 30_000 });
  await page.getByRole("tab", { name: "Tools" }).click();
  await expect(page.getByRole("heading", { name: "Skills" })).toBeVisible({ timeout: 30_000 });
}

test.beforeEach(async ({ page, request }) => {
  await suppressTour(page);
  await removeAgent(request);
  const created = await request.post("/api/v1/company/team", {
    data: { name: AGENT_NAME, role: "Probe" },
  });
  expect(created.ok(), `creating ${AGENT_NAME} failed: ${await created.text()}`).toBeTruthy();
  AGENT_ID = (await created.json()).id;
});

test.afterEach(async ({ request }) => {
  await removeAgent(request);
});

test("an inherited scope reads every enabled skill, and narrows one switch at a time", async ({
  page,
  request,
}) => {
  const enabled = await enabledSlugs(request);
  expect(enabled.length, "the harness company should have enabled skills").toBeGreaterThan(1);
  const dropped = enabled[0];
  const kept = enabled.slice(1);

  await openScopeCard(page);

  // Inheriting: no scope of its own, so it reads the company's whole set.
  await expect(page.getByTestId("agent-skills-effective")).toBeVisible();
  for (const slug of enabled) {
    await expect(page.getByTestId("agent-skills-effective")).toContainText(slug);
  }
  await expect(page.getByText("reads every skill the company has enabled")).toBeVisible();

  await page.getByTestId("agent-skills-edit").click();
  await expect(page.getByTestId("agent-skills-toggles")).toBeVisible();

  // Every switch reads on, though the stored scope is nothing at all.
  for (const slug of enabled) {
    await expect(page.getByTestId(`agent-skill-toggle-${slug}`)).toHaveAttribute(
      "aria-checked",
      "true",
    );
  }

  // Saving an untouched inherited view would store the whole enabled set as an
  // explicit list and turn an inheriting teammate into a pinned one, so there
  // is nothing to save until a switch has actually moved.
  await expect(page.getByTestId("agent-skills-save")).toBeDisabled();

  await page.getByTestId(`agent-skill-toggle-${dropped}`).click();
  await expect(page.getByTestId(`agent-skill-toggle-${dropped}`)).toHaveAttribute(
    "aria-checked",
    "false",
  );
  // The untouched switches stay on — this is the assertion the `touched` flag
  // exists for.
  for (const slug of kept) {
    await expect(page.getByTestId(`agent-skill-toggle-${slug}`)).toHaveAttribute(
      "aria-checked",
      "true",
    );
  }

  await page.getByTestId("agent-skills-save").click();
  await expect(page.getByTestId("agent-skills-editor")).toHaveCount(0, { timeout: 30_000 });

  const effective = page.getByTestId("agent-skills-effective");
  for (const slug of kept) await expect(effective).toContainText(slug);
  await expect(effective).not.toContainText(dropped);
  await expect(page.getByText("narrowed by what the company has enabled")).toBeVisible();

  // And the host agrees, rather than the card having narrowed only on screen.
  const stored = await request.get(`/api/v1/company/team/${AGENT_ID}`);
  expect((await stored.json()).skills.requested).toEqual(kept);
});

test("an explicit empty scope is warned about, and Reset puts every skill back", async ({
  page,
  request,
}) => {
  const enabled = await enabledSlugs(request);
  await openScopeCard(page);

  await page.getByTestId("agent-skills-edit").click();
  for (const slug of enabled) {
    await page.getByTestId(`agent-skill-toggle-${slug}`).click();
  }

  // An empty draft is a deliberate no-skills scope, not a reset — and the card
  // says which, because the two are one click apart.
  await expect(page.getByTestId("agent-skills-empty-warning")).toBeVisible();
  await expect(page.getByTestId("agent-skills-empty-warning")).toContainText(
    "Reset to every skill",
  );

  await page.getByTestId("agent-skills-save").click();
  await expect(page.getByTestId("agent-skills-editor")).toHaveCount(0, { timeout: 30_000 });
  await expect(page.getByTestId("agent-skills-empty")).toContainText(
    "it was given an explicit empty scope",
  );
  expect(
    (await (await request.get(`/api/v1/company/team/${AGENT_ID}`)).json()).skills.requested,
  ).toEqual([]);

  // Reset is offered only once there is a stored scope to reset, and it stores
  // nothing rather than storing the enabled set.
  await page.getByTestId("agent-skills-edit").click();
  await page.getByTestId("agent-skills-reset").click();
  await expect(page.getByTestId("agent-skills-editor")).toHaveCount(0, { timeout: 30_000 });

  const effective = page.getByTestId("agent-skills-effective");
  for (const slug of enabled) await expect(effective).toContainText(slug);
  expect(
    (await (await request.get(`/api/v1/company/team/${AGENT_ID}`)).json()).skills.requested,
  ).toBeNull();
});

test("a stored slug the company has not enabled is named rather than hidden", async ({
  page,
  request,
}) => {
  const enabled = await enabledSlugs(request);
  const held = enabled[0];
  const retired = "retired-playbook";

  // A scope entry the company does not have enabled is stored rather than
  // refused — the picker renders against a set fetched at page load, and a
  // concurrent uninstall would otherwise fail an honest save. So the only
  // thing between an operator and a scope that quietly confers nothing is the
  // card saying so.
  const patched = await request.patch(`/api/v1/company/team/${AGENT_ID}`, {
    data: { skills: [held, retired] },
  });
  expect(patched.ok(), `scoping ${AGENT_ID} failed: ${await patched.text()}`).toBeTruthy();

  await openScopeCard(page);

  const dropped = page.getByTestId("agent-skills-dropped");
  await expect(dropped).toBeVisible();
  await expect(dropped).toContainText(retired);
  // A slug the teammate does hold is not a dropped one.
  await expect(dropped).not.toContainText(held);
  await expect(page.getByTestId("agent-skills-effective")).toContainText(held);
});

test("a member reads the scope and is offered no way to change it", async ({
  page,
  browser,
}) => {
  const memberContext = await browser.newContext({ storageState: undefined });
  try {
    await signInAsMember(page.request, memberContext.request, MEMBER_EMAIL);
    const memberPage = await memberContext.newPage();
    await suppressTour(memberPage);
    await openScopeCard(memberPage);

    // The scope is readable: what a teammate may read explains what it can do.
    await expect(memberPage.getByTestId("agent-skills-effective")).toBeVisible({
      timeout: 30_000,
    });
    // `skills` is admin-only on the host, so the console must not invite the
    // refusal — the editor is not offered at all.
    await expect(memberPage.getByTestId("agent-skills-edit")).toHaveCount(0);
    await expect(memberPage.getByTestId("agent-skills-editor")).toHaveCount(0);
  } finally {
    await memberContext.close();
  }
});
