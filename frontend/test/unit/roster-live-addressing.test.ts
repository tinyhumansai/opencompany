import { readFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import { describe, expect, it } from "vitest";

import type { TeamMemberDto } from "@/api/types";
import { rosterIdentity } from "@/lib/team";

/**
 * Bug B-030: a teammate you hire does real work the console never shows you.
 *
 * Hire "Priya", DM her, she replies. `GET …/chat/history?desk=priya` returns
 * the reply — 2 rows, one of them hers, plus the workspace note she wrote — and
 * the DM shows only your own message, for as long as the tab stays open.
 *
 * The cause is not a hardcoded desk list. `ChatView`'s rail is derived from the
 * live roster it keeps for itself, which is why Priya is offered in the New
 * message picker and counted in "Show teammates" the moment the host confirms
 * the write. What was fixed once and then closed over is the **shell's** copy:
 * `AppShell`'s `[client, company]` effect read `/team` once, and from that one
 * snapshot built the `chat/history` poll targets *and* the thread-to-channel
 * map a live SSE frame is routed through. A teammate hired after that read was
 * therefore in neither: their desk was never polled, and `channelForThread`
 * dropped their live frames for want of a channel. The console showed the
 * operator's own optimistic line and nothing else — while the company billed
 * them for the work.
 *
 * Two things have to hold for that to stay fixed, and they are tested
 * separately below because they fail separately:
 *
 * 1. The shell can tell a changed roster from an unchanged one cheaply, or the
 *    five-second history poll would re-render the whole shell forever.
 * 2. The poll actually re-reads the roster and re-derives from it, rather than
 *    calling the closure built at mount.
 */

describe("rosterIdentity", () => {
  const member = (id: string, name?: string): TeamMemberDto =>
    ({ id, name, role: "Role" }) as TeamMemberDto;

  it("is stable across two reads of an unchanged roster", () => {
    // The property the five-second poll depends on: a fresh parse of the same
    // roster is a different array of different objects every tick, so identity
    // comparison would re-derive — and re-render — on every tick forever.
    const first = [member("writer", "Writer"), member("priya", "Priya")];
    const second = [member("writer", "Writer"), member("priya", "Priya")];

    expect(rosterIdentity(second)).toBe(rosterIdentity(first));
  });

  it("changes when a teammate is hired", () => {
    const before = [member("writer", "Writer")];
    const after = [member("writer", "Writer"), member("priya", "Priya")];

    expect(rosterIdentity(after)).not.toBe(rosterIdentity(before));
  });

  it("changes when a teammate leaves", () => {
    const before = [member("writer", "Writer"), member("priya", "Priya")];
    const after = [member("writer", "Writer")];

    expect(rosterIdentity(after)).not.toBe(rosterIdentity(before));
  });

  it("changes when one teammate is swapped for another", () => {
    // The case a row count cannot see. Hiring one teammate and dropping another
    // between two ticks leaves the length identical while the addressing has
    // completely moved — the new hire's desk would never be polled.
    const before = [member("writer", "Writer"), member("priya", "Priya")];
    const after = [member("writer", "Writer"), member("nadia", "Nadia")];

    expect(after).toHaveLength(before.length);
    expect(rosterIdentity(after)).not.toBe(rosterIdentity(before));
  });

  it("changes when a teammate is renamed", () => {
    // Addressing is unaffected by a rename; the name map a live receipt
    // resolves an agent id through is not. The shell derives both from this
    // read, so a rename has to count as a change or "Priya says…" keeps
    // rendering the old name until the tab is reloaded.
    const before = [member("priya", "Priya")];
    const after = [member("priya", "Priya Raman")];

    expect(rosterIdentity(after)).not.toBe(rosterIdentity(before));
  });

  // Codex review, PR #2054: `fromDto` (`@/lib/team`) falls back to `role` as
  // the displayed name when a teammate carries no explicit `name` — the
  // fingerprint has to follow that same fallback, or a role change on a
  // nameless teammate is invisible to it and the shell never re-derives the
  // DM rail, the live-reply name map or the mention directory for them.
  it("changes when a nameless teammate's role changes", () => {
    const before = [{ id: "priya", name: undefined, role: "Engineer" } as TeamMemberDto];
    const after = [{ id: "priya", name: undefined, role: "Growth" } as TeamMemberDto];

    expect(rosterIdentity(after)).not.toBe(rosterIdentity(before));
  });

  it("changes when the roster is reordered", () => {
    // Roster order is the order DM threads are built in, so a reorder really
    // does change what renders.
    const before = [member("writer", "Writer"), member("priya", "Priya")];
    const after = [member("priya", "Priya"), member("writer", "Writer")];

    expect(rosterIdentity(after)).not.toBe(rosterIdentity(before));
  });

  it("keeps two rosters apart when a field boundary falls inside a value", () => {
    // A `,`-joined fingerprint would call these the same roster: one teammate
    // whose name contains the separator, against two teammates. The separators
    // are ASCII unit/record separator precisely because neither can occur in a
    // host-issued id or a name typed into the add-teammate dialog.
    const one = [member("a", "x,y")];
    const two = [member("a", "x"), member("y", undefined)];

    expect(rosterIdentity(one)).not.toBe(rosterIdentity(two));
  });

  it("fingerprints an empty roster without throwing", () => {
    expect(rosterIdentity([])).toBe("");
  });
});

/**
 * `AppShell` is too large, and pulls in too much (SSE, the authenticated
 * client, routing) to mount in a unit test — `chat-realtime-poll.test.ts` and
 * `chat-receipt-scope-reset.test.ts` both settle the same way, reading the
 * source and asserting on the literal wiring. What this locks down is that the
 * roster re-read is on the *polling* path: `rosterIdentity` being correct buys
 * nothing if the timer still calls the closure built at mount.
 */
const here = dirname(fileURLToPath(import.meta.url));
const appShell = readFileSync(resolve(here, "../../src/components/app-shell.tsx"), "utf8");

describe("the shell's chat addressing is re-derived from the live roster", () => {
  it("hands the recurring poll the callback that re-reads the roster", () => {
    // The one line the whole bug turns on. Before the fix this passed
    // `rehydrateAll`, whose target list was fixed at mount.
    expect(appShell).toMatch(
      /disposeRehydratePolling = startVisiblePolling\(refreshAll, 5000\);/,
    );
  });

  it("re-reads /team on the tick and re-derives when the roster moved", () => {
    const start = appShell.indexOf("const refreshAll = () => {");
    expect(start, "the polling callback").toBeGreaterThan(-1);
    const body = appShell.slice(start, appShell.indexOf("startVisiblePolling", start));

    expect(body).toContain("client\n              .listTeam(company)");
    expect(body).toMatch(/const key = rosterIdentity\(members\);/);
    expect(body).toMatch(/if \(key === rosterKey\) return;/);
    expect(body).toMatch(/applyRoster\(members\);/);
  });

  it("still rehydrates every tick, roster change or not", () => {
    // The history poll is the older job and must not become conditional on the
    // roster having moved — a persisted message on an existing desk still has
    // to be recovered when a live frame was missed.
    const start = appShell.indexOf("const refreshAll = () => {");
    const body = appShell.slice(start, appShell.indexOf("startVisiblePolling", start));
    const guarded = body.indexOf("if (!rosterReadInFlight)");
    const rehydrate = body.indexOf("rehydrateAll();");

    expect(guarded).toBeGreaterThan(-1);
    expect(rehydrate).toBeGreaterThan(-1);
    // Outside the in-flight guard's block, i.e. after it closes.
    expect(body.slice(rehydrate)).not.toContain("rosterReadInFlight = true");
  });

  it("derives the poll targets through the same function the re-read calls", () => {
    // Both the mount-time pass and every later roster change go through one
    // derivation, so the two cannot drift into disagreeing about where a
    // teammate's transcript is fetched from.
    expect(appShell).toMatch(/const applyRoster = \(members: TeamMemberDto\[\]\) => \{/);
    expect(appShell).toMatch(/applyRoster\(team\);\n\s*rosterKey = rosterIdentity\(team\);/);
    expect(appShell).toMatch(
      /const rehydrateAll = \(\) => rehydrateTargets\(targets\.threadIds, targets\.channels\);/,
    );
  });

  it("skips a roster read while one is already in flight", () => {
    // Same rule `hydrateThread` applies per thread: a slow `/team` must not let
    // ticks stack into a queue of duplicate reads.
    expect(appShell).toMatch(/let rosterReadInFlight = false;/);
    expect(appShell).toMatch(/rosterReadInFlight = false;\n\s*\}\);/);
  });
});

/**
 * Defect B-071: B-030 fixed the poll set; the ADDRESSING set stayed a snapshot.
 *
 * A teammate hired in a second tab became readable and unaddressable at the
 * same time. Their turn output arrived live — that is B-030's fix working — and
 * the `@`-mention picker in the open tab never listed them. Typing `@Rafi`
 * matched nothing, and the fall-through is the part that makes this a P2 rather
 * than a cosmetic gap: a loaded-but-stale directory is truthy, so the composer
 * resolves no span and sends an explicit `mentions: []`, which the wire
 * contract defines as "the directory resolved none — do NOT extract from the
 * text". Omitting the field would have let the host find Rafi. Sending an empty
 * one silences it, the line reaches the channel as plain prose, and the
 * channel's catch-all agent answers it *under Rafi's name*.
 *
 * The fix is one signal — the shell's roster fingerprint, which it already
 * computes for the poll — published to every structure derived from the roster.
 * These tests read the source for the same reason the block above does:
 * `AppShell` and `ChatView` are too large and too connected to mount here.
 */
const chatView = readFileSync(resolve(here, "../../src/views/ChatView.tsx"), "utf8");

describe("the roster fingerprint reaches everything derived from the roster (B-071)", () => {
  it("publishes the fingerprint the poll already computes", () => {
    // Before the fix `rosterIdentity` was computed on the polling path and
    // compared against a local, then thrown away. Nothing outside that closure
    // could learn the roster had moved.
    expect(appShell).toMatch(/const \[rosterEpoch, setRosterEpoch\] = useState\(""\);/);
    expect(appShell).toMatch(/setRosterEpoch\(rosterIdentity\(members\)\);/);
  });

  it("re-derives the shell's own people map on it", () => {
    // The label map behind every typing line and the People section. A person
    // the directory did not name at mount was dropped from both forever.
    const at = appShell.indexOf("}, [client, company, rosterEpoch]);");
    expect(at, "the companyPeople effect is not keyed on the roster").toBeGreaterThan(-1);
  });

  it("hands the fingerprint to the chat view", () => {
    // The half that was missing: the prop existed and the shell never passed
    // it, so the view's own re-derivation was unreachable code.
    expect(appShell).toMatch(/rosterEpoch=\{rosterEpoch\}/);
  });

  it("re-derives all three of the chat view's roster reads from it", () => {
    const start = chatView.indexOf("if (rosterEpochActedOn.current === rosterEpoch) return;");
    expect(start, "the epoch effect").toBeGreaterThan(-1);
    const body = chatView.slice(start, chatView.indexOf("}, [rosterEpoch", start));

    // The DM rail's teammate list.
    expect(body).toContain("void boot();");
    // The `@`-mention directory — the one that causes the wrong answer.
    expect(body).toContain("reloadDirectory();");
    // The channels, whose `memberIds` are roster agent ids and so decide
    // `inChannel`: membership in the pane, ranking in the picker, and the
    // "not on this channel" warning that blocks the first send.
    expect(body).toContain("void loadDesks(true);");
  });

  it("does not re-read on the mount epoch, which the boot read already covers", () => {
    // Acting on the first epoch seen would double every mount's `/team` and
    // `/mentionables` request, for a roster that has not moved.
    expect(chatView).toMatch(/const rosterEpochActedOn = useRef<string \| null>\(null\);/);
    expect(chatView).toMatch(
      /if \(rosterEpochActedOn\.current === null\) \{\s*rosterEpochActedOn\.current = rosterEpoch;\s*return;\s*\}/,
    );
    // And the marker is cleared by the same effect that fires `boot()`, so the
    // two cannot drift apart.
    const bootEffect = chatView.indexOf("rosterEpochActedOn.current = null;");
    expect(bootEffect).toBeGreaterThan(-1);
    expect(chatView.slice(bootEffect, bootEffect + 120)).toContain("void boot();");
  });

  it("refreshes the channels without tearing the rail down", () => {
    // A background re-read has a correct rail on screen. Blanking it to a
    // spinner would make someone else's hire look like a fault in this tab —
    // and a failed refresh must keep the desks it already has rather than
    // replace them with an error.
    expect(chatView).toMatch(/async \(quiet = false\) => \{/);
    expect(chatView).toMatch(/if \(!quiet\) \{\s*setDesks\(null\);/);
    expect(chatView).toMatch(/if \(quiet\) return;\s*setDesksError\(/);
  });

  it("stays inert for a caller that has no roster poll", () => {
    // The prop is optional so a test or an embed keeps the old behaviour
    // rather than crashing — but an absent epoch must not be read as a
    // *changed* one, which would refetch on every render.
    expect(chatView).toMatch(/if \(rosterEpoch === undefined\) return;/);
  });
});
