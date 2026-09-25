// Shared ground for the Skills specs: where the tab lives, how a skill
// document is built, and how one is taken back out again.
//
// The console's Skills surfaces write to a host the whole suite shares, so
// every spec here creates named skills and removes them through the API rather
// than leaving them for the next file to trip over. `slug` is the contract
// between the two halves: the host derives it from the document's own `name`
// (`skill_upload::read_markdown` → `slugify`), so a spec that knows the name
// knows what to clean up.

import { expect, type APIRequestContext, type Page } from "@playwright/test";

/** The Skills tab's address. Bare `#/skills` no longer names a view. */
export const SKILLS_URL = "/#/settings/skills";

/** The company's effective skills, as the host serves them. */
export interface HostSkill {
  id: string;
  name: string;
  description: string;
  category: string;
  source: string;
  enabled: boolean;
  version?: string | null;
  updatedAtMillis?: number | null;
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
export async function suppressTour(page: Page) {
  await page.addInitScript(() => {
    const seen = JSON.stringify({ skipped: true, seenAt: Date.now() });
    for (const key of ["oc-tour:single", "oc-tour:e2e-harness-co", "oc-tour:null"]) {
      window.localStorage.setItem(key, seen);
    }
  });
}

/** Opens the Skills tab and waits for the list to have rendered. */
export async function openSkills(page: Page) {
  await page.goto(SKILLS_URL);
  await expect(page.getByTestId("skills-read-only-note")).toBeVisible({ timeout: 30_000 });
}

/** The installed row for `name`. */
export function installedCard(page: Page, name: string) {
  return page.getByTestId("installed-card").filter({ hasText: name });
}

/** The company's effective skill set, read straight from the host. */
export async function hostSkills(request: APIRequestContext): Promise<HostSkill[]> {
  const answer = await request.get("/api/v1/company/skills");
  expect(answer.ok(), `GET …/skills failed: ${answer.status()}`).toBeTruthy();
  return (await answer.json()) as HostSkill[];
}

/**
 * Removes `slug` if it is installed, so a spec can be run twice.
 *
 * A built-in is refused by the host and a slug that was never installed is a
 * 404; neither is a failure of the spec that called this, so both are ignored.
 */
export async function removeSkill(request: APIRequestContext, slug: string) {
  await request
    .post(`/api/v1/company/skills/${encodeURIComponent(slug)}/uninstall`)
    .catch(() => undefined);
}

/** A `SKILL.md` with the frontmatter the host's validator requires. */
export function skillDoc(fields: {
  name: string;
  description: string;
  category?: string;
  body?: string;
}): string {
  const category = fields.category ?? "Ops";
  const body = fields.body ?? "Do the thing, then say what was done.";
  return [
    "---",
    `name: ${fields.name}`,
    `description: ${fields.description}`,
    `category: ${category}`,
    "---",
    "",
    body,
    "",
  ].join("\n");
}

/** What `setInputFiles` wants for a document held in memory. */
export function markdownUpload(filename: string, doc: string) {
  return { name: filename, mimeType: "text/markdown", buffer: Buffer.from(doc, "utf8") };
}

/** What `setInputFiles` wants for an archive built in memory. */
export function archiveUpload(filename: string, slug: string, doc: string) {
  return { name: filename, mimeType: "application/zip", buffer: zipOneSkill(slug, doc) };
}

/**
 * A zip holding exactly `<slug>/SKILL.md` and nothing else.
 *
 * Built here rather than committed as a binary fixture: the host reads the
 * archive's single directory as the skill's slug, so the name under test and
 * the bytes uploaded stay one decision. Entries are stored rather than
 * deflated — the reader accepts either, and `zlib` would only add a dependency
 * on a compression level to reason about.
 */
export function zipOneSkill(slug: string, doc: string): Buffer {
  const name = `${slug}/SKILL.md`;
  const nameBytes = Buffer.from(name, "utf8");
  const data = Buffer.from(doc, "utf8");
  const crc = crc32(data);

  const local = Buffer.alloc(30);
  local.writeUInt32LE(0x04034b50, 0);
  local.writeUInt16LE(20, 4); // version needed
  local.writeUInt16LE(0, 6); // flags
  local.writeUInt16LE(0, 8); // stored
  local.writeUInt16LE(0, 10); // time
  local.writeUInt16LE(0x21, 12); // date — 1 Jan 1980, the epoch of the format
  local.writeUInt32LE(crc, 14);
  local.writeUInt32LE(data.length, 18);
  local.writeUInt32LE(data.length, 22);
  local.writeUInt16LE(nameBytes.length, 26);
  local.writeUInt16LE(0, 28); // extra

  const central = Buffer.alloc(46);
  central.writeUInt32LE(0x02014b50, 0);
  central.writeUInt16LE(20, 4); // version made by
  central.writeUInt16LE(20, 6); // version needed
  central.writeUInt16LE(0, 8);
  central.writeUInt16LE(0, 10);
  central.writeUInt16LE(0, 12);
  central.writeUInt16LE(0x21, 14);
  central.writeUInt32LE(crc, 16);
  central.writeUInt32LE(data.length, 20);
  central.writeUInt32LE(data.length, 24);
  central.writeUInt16LE(nameBytes.length, 28);
  central.writeUInt16LE(0, 30); // extra
  central.writeUInt16LE(0, 32); // comment
  central.writeUInt16LE(0, 34); // disk
  central.writeUInt16LE(0, 36); // internal attrs
  central.writeUInt32LE((0o100644 << 16) >>> 0, 38); // external attrs: a regular file
  central.writeUInt32LE(0, 42); // offset of the local header

  const centralSize = central.length + nameBytes.length;
  const end = Buffer.alloc(22);
  end.writeUInt32LE(0x06054b50, 0);
  end.writeUInt16LE(0, 4);
  end.writeUInt16LE(0, 6);
  end.writeUInt16LE(1, 8);
  end.writeUInt16LE(1, 10);
  end.writeUInt32LE(centralSize, 12);
  end.writeUInt32LE(local.length + nameBytes.length + data.length, 16);
  end.writeUInt16LE(0, 20);

  return Buffer.concat([local, nameBytes, data, central, nameBytes, end]);
}

const CRC_TABLE = (() => {
  const table = new Uint32Array(256);
  for (let i = 0; i < 256; i += 1) {
    let value = i;
    for (let bit = 0; bit < 8; bit += 1) {
      value = value & 1 ? 0xedb88320 ^ (value >>> 1) : value >>> 1;
    }
    table[i] = value >>> 0;
  }
  return table;
})();

function crc32(bytes: Buffer): number {
  let crc = 0xffffffff;
  for (const byte of bytes) crc = CRC_TABLE[(crc ^ byte) & 0xff] ^ (crc >>> 8);
  return (crc ^ 0xffffffff) >>> 0;
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
export async function signInAsMember(
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
