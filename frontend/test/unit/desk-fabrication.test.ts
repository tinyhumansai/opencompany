import { readFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import { describe, expect, it } from "vitest";

import type { ApprovalSummary } from "@/api/types";
import { approvalThreadLink } from "@/components/approval-card";
import type { DeskDto } from "@/api/types";
import { GENERAL_CHANNEL } from "@/lib/desks";
import { MAIN_THREAD_ID } from "@/lib/chat";
import type { TeamMember } from "@/lib/team";
import { threadsFromDesks } from "@/lib/threads";
import { buildChannels } from "@/views/chat/model";

/**
 * A company with no desks is shown as a company with no desks.
 *
 * The console used to answer an empty `/desks` list with `defaultDesks()` — a
 * fabricated Strategy desk, Creative studio and Front desk. Three surfaces did
 * it (`ChatView`, `AppShell`, `approval-card`), so a company that had never
 * declared a `[[group_chat]]` showed three channels it did not have, could not
 * open, and whose ids a real channel could later collide with. The overview
 * graph has no such fallback and correctly drew "No desks yet" — the two
 * surfaces disagreeing about one read is how this was noticed.
 *
 * The distinction that replaces it: an **answer** is taken as given, empty or
 * not; only a host that never answered (no `/desks` route at all — the pre-#53
 * shape) still gets the static set. This is issue #370's rule applied to the
 * answer rather than to the failure.
 */

const SRC = resolve(dirname(fileURLToPath(import.meta.url)), "../../src");
const read = (rel: string) => readFileSync(resolve(SRC, rel), "utf8");

/** The names the fabricated set used to put in front of an operator. */
const FABRICATED = ["Strategy desk", "Creative studio", "Front desk"];

const NO_MEMBERS: TeamMember[] = [];

describe("a company with no desks (empty /desks answer)", () => {
  it("gets the main line and nothing else in the chat list", () => {
    const threads = threadsFromDesks([]);

    expect(threads.map((t) => t.id)).toEqual([MAIN_THREAD_ID]);
    for (const name of FABRICATED) {
      expect(threads.map((t) => t.contact.name)).not.toContain(name);
    }
  });

  it("still lists a desk the host does return", () => {
    const desks: DeskDto[] = [
      {
        id: "engineering",
        name: "Engineering desk",
        description: "How things are built",
        members: ["engineer"],
      },
    ];

    expect(threadsFromDesks(desks).map((t) => t.id)).toEqual([MAIN_THREAD_ID, "engineering"]);
  });

  it("builds a rail of #general and nothing beside it", () => {
    const channels = buildChannels(NO_MEMBERS, []).flatMap((section) => section.channels);

    expect(channels.map((c) => c.id)).toEqual([MAIN_THREAD_ID]);
  });

  it("keeps #general resolvable for an approval raised on the main line", () => {
    // The empty list is an answer, so the one channel every company has can be
    // named. While `[]` also meant "the read failed" this label was withheld.
    const approval = {
      id: "a1",
      kind: "runtime.unlabelled_effect",
      amount_usd: null,
      at_millis: 0,
      agent: null,
      thread: MAIN_THREAD_ID,
    } as ApprovalSummary;

    expect(approvalThreadLink(approval, [], NO_MEMBERS)).toEqual({
      channelId: MAIN_THREAD_ID,
      label: `#${GENERAL_CHANNEL}`,
    });
  });

  it("withholds the link when the desks read failed rather than answered", () => {
    // `null`, not `[]` — an unknown topology must not be guessed at, because
    // `ChatView` renders no rail behind a failed read and the link would land
    // nowhere.
    const approval = {
      id: "a1",
      kind: "runtime.unlabelled_effect",
      amount_usd: null,
      at_millis: 0,
      agent: null,
      thread: MAIN_THREAD_ID,
    } as ApprovalSummary;

    expect(approvalThreadLink(approval, null, NO_MEMBERS)).toBeNull();
  });
});

describe("no surface fabricates desks over an answered read", () => {
  it("ChatView keeps the host's list, and falls back only on a 404", () => {
    const src = read("views/ChatView.tsx");

    expect(src).toContain("setDesks(dtos.map(deskFromDto));");
    expect(src).not.toContain("dtos.length ? dtos.map(deskFromDto) : defaultDesks()");
    // The 404 leg — a host with no `/desks` route at all — still stands in.
    expect(src).toContain("error.status === 404");
    expect(src).toContain("setDesks(defaultDesks());");
  });

  it("AppShell keeps the host's list on the answered leg", () => {
    const src = read("components/app-shell.tsx");

    // `desks` is `null`, not merely absent, when this leg runs: the Operator
    // feed fetch (issue #1757) is read in parallel via `Promise.all`, each
    // leg's own `.catch(() => null)` so one failing does not sink the other.
    // A per-item failure therefore cannot reach the whole-chain `.catch`
    // below, so the null check is what stands in for it — an answered-but-
    // empty array must still flow to `desks.map(deskFromDto)` untouched.
    expect(src).toContain("const chatDesks = desks === null ? defaultDesks() : desks.map(deskFromDto);");
    expect(src).not.toContain("desks.length ? desks.map(deskFromDto) : defaultDesks()");
    // Its `.catch` leg is the one place the static set is still right: nothing
    // was answered there at all.
    expect(src).toContain("const fallbackDesks = defaultDesks();");
  });

  it("the approvals topology read no longer imports the fabricated set", () => {
    const src = read("components/approval-card.tsx");

    // The import is the check, not a mention: the file still *explains* the
    // fabricated set in prose, and should.
    expect(src).not.toMatch(/import \{[^}]*\bdefaultDesks\b[^}]*\} from "@\/lib\/desks"/);
    expect(src).toContain(".catch(() => null)");
  });

  it("buildChannels defaults to no desks rather than to the trio", () => {
    const src = read("views/chat/model.ts");

    expect(src).not.toContain("desks: Desk[] = defaultDesks()");
    expect(src).toContain("desks: Desk[] = []");
  });
});

describe("defaultDesks itself", () => {
  it("is reachable only from the two never-answered legs", () => {
    // A grep guard rather than a behavioural one: the fabricated set is still
    // correct for a host that has no `/desks` route, so it cannot simply be
    // deleted — what matters is that nothing else reaches for it.
    // Import sites, not mentions — every one of these files discusses the
    // fabricated set in a comment, and the comments are the point.
    const imports = (src: string) =>
      /import \{[^}]*\bdefaultDesks\b[^}]*\} from "@\/lib\/desks"/.test(src) ||
      /^\s*defaultDesks,$/m.test(src);
    const callers = ["views/ChatView.tsx", "components/app-shell.tsx"];
    const others = ["components/approval-card.tsx", "views/chat/model.ts"];

    for (const file of callers) expect(imports(read(file)), file).toBe(true);
    for (const file of others) expect(imports(read(file)), file).toBe(false);
    // `threads.ts` declares `defaultThreads` next to it and imports neither.
    expect(read("lib/threads.ts")).not.toContain('from "@/lib/desks"');
  });
});

describe("the fabricated set is still what it was", () => {
  it("names the three desks the fallback legs stand in", () => {
    // If these names change, the assertions above that spell them out are
    // testing a set that no longer exists.
    const src = read("lib/desks.ts");
    for (const name of FABRICATED) expect(src).toContain(name);
  });
});
