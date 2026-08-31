import { readFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import { describe, expect, it } from "vitest";

import type { DeskDto } from "@/api/types";
import { buildOrgTree } from "@/lib/org";
import type { TeamMember } from "@/lib/team";
import { buildChannels, deskFromDto } from "@/views/chat/model";

/**
 * The console half of the `auto` channel (issue #1835).
 *
 * An `auto` channel has **no lead**: `members[0]` is the host's order, not a
 * rank — the host's own `desk_lead` is `None` for it by definition — and its
 * answerer is picked per message. Three consumers used to derive a lead from
 * position alone, and each is pinned here: `deskFromDto` must carry the mode
 * at all (dropping a DTO field silently is how issue #369 lost memberships),
 * `buildChannels` must flag the channel so `ChatView` withholds `leadId`, and
 * `buildOrgTree` must not crown seat zero.
 *
 * Every assertion has a paired one on a lead desk, because the failure that
 * matters most is the quiet one in the other direction: a mode nobody stated
 * must keep behaving exactly as before the field existed.
 */

function desk(over: Partial<DeskDto> & Pick<DeskDto, "id" | "name">): DeskDto {
  return {
    members: ["engineer", "designer"],
    ...over,
  };
}

function member(over: Partial<TeamMember> & Pick<TeamMember, "id" | "name">): TeamMember {
  return {
    role: "Engineer",
    description: "",
    tone: "sky",
    avatar: "green",
    inboxEnabled: false,
    effectiveTools: [],
    desks: [],
    ...over,
  };
}

const ROSTER: TeamMember[] = [
  member({ id: "engineer", name: "Backend Engineer" }),
  member({ id: "designer", name: "Product Designer", role: "Designer" }),
];

describe("deskFromDto", () => {
  it("carries the responder mode, and its absence", () => {
    expect(deskFromDto(desk({ id: "launch", name: "Launch", responder: "auto" })).responder).toBe(
      "auto",
    );
    // A desk that never states a mode stays undefined — not defaulted to a
    // string here, so `d.responder === "auto"` is the only truthy read.
    expect(deskFromDto(desk({ id: "eng", name: "Engineering" })).responder).toBeUndefined();
  });
});

describe("buildChannels", () => {
  it("flags an auto channel leadless and leaves lead desks alone", () => {
    const [channels] = buildChannels(ROSTER, [
      deskFromDto(desk({ id: "eng", name: "Engineering" })),
      deskFromDto(desk({ id: "launch", name: "Launch", responder: "auto" })),
    ]);
    // Selected by id, not by index: since issue #1743 the rail leads with the
    // built-in `#general` row, which is not a desk and shifts every position.
    const byId = (id: string) => channels.channels.find((c) => c.id === id)!;
    const eng = byId("eng");
    const launch = byId("launch");
    expect(launch.leadless).toBe(true);
    // Undefined rather than false, so every pre-#1835 consumer that never
    // reads the flag serializes and compares exactly as it did.
    expect(eng.leadless).toBeUndefined();
  });

  it("states the routing rule when an auto channel has no blurb, and lets a blurb win", () => {
    const [channels] = buildChannels(ROSTER, [
      deskFromDto(desk({ id: "launch", name: "Launch", responder: "auto" })),
      deskFromDto(
        desk({ id: "beta", name: "Beta", responder: "auto", description: "Beta rollout." }),
      ),
      deskFromDto(desk({ id: "eng", name: "Engineering" })),
    ]);
    const byId = (id: string) => channels.channels.find((c) => c.id === id)!;
    const launch = byId("launch");
    const beta = byId("beta");
    const eng = byId("eng");
    expect(launch.purpose).toBe("Best fit picks up anything you don't @-mention");
    expect(beta.purpose).toBe("Beta rollout.");
    // A lead desk with no blurb keeps its empty purpose — the routing line is
    // a claim about auto channels only.
    expect(eng.purpose).toBe("");
  });
});

describe("buildOrgTree", () => {
  const roster = [
    { id: "engineer", name: "Backend Engineer", role: "Engineer" },
    { id: "designer", name: "Product Designer", role: "Designer" },
  ];

  it("crowns no seat on an auto channel, and still crowns a lead desk's first seat", () => {
    const tree = buildOrgTree(
      "Acme",
      [
        desk({ id: "eng", name: "Engineering" }),
        desk({ id: "launch", name: "Launch", responder: "auto" }),
      ],
      roster,
    );
    const [eng, launch] = tree.desks;
    expect(eng.seats.map((s) => s.lead)).toEqual([true, false]);
    expect(launch.seats.map((s) => s.lead)).toEqual([false, false]);
  });
});

/**
 * The three review findings on #1872, pinned in the source-contract idiom
 * `chat-rail-focus.test.ts` established for shell wiring a jsdom render
 * cannot reach (the dialog's submit and `ChatView`'s create callback both
 * need the whole client and every hook to render).
 */
describe("channel creation guards (#1872 review)", () => {
  const here = dirname(fileURLToPath(import.meta.url));
  const read = (rel: string) => readFileSync(resolve(here, "../../src", rel), "utf8");
  const dialog = () => read("views/chat/ChannelCreateDialog.tsx");
  const chatView = () => read("views/ChatView.tsx");

  it("refuses to submit an auto channel with no members, with a field-level reason", () => {
    const src = dialog();
    // The gate sits in submit(), before the POST — a channel with nobody in
    // it has nobody to answer, and the host refuses it too, so the dialog
    // must not offer a hopeful submit.
    const gate = src.indexOf("if (chosen.length === 0) {");
    expect(gate).toBeGreaterThan(-1);
    expect(src.slice(gate, gate + 400)).toContain("setMembersError(");
    // Field-level, not the whole-form banner: the complaint renders at the
    // members section and is cleared the moment a member is toggled in.
    expect(src).toContain("{membersError && (");
    const toggle = src.indexOf("function toggle(id: string)");
    expect(src.slice(toggle, toggle + 300)).toContain("setMembersError(null)");
    // And the POST no longer sends an absent members list at all.
    expect(src).toContain("members: chosen,");
    expect(src).not.toContain("chosen.length > 0 ? chosen : undefined");
  });

  it("drops a createDesk completion that lands after a scope switch", () => {
    const src = dialog();
    // Captured at submit, compared at completion — the same shape ChatView's
    // send path uses against its scopeRef. A create that resolves after the
    // operator moved company must not hand its desk to the new scope's rail.
    expect(src).toContain("const scopeAtSubmit = scopeNow.current;");
    const guard = src.indexOf("scopeNow.current.client !== scopeAtSubmit.client");
    expect(guard).toBeGreaterThan(-1);
    expect(src.slice(guard, guard + 300)).toContain("return;");
    // The guard sits between the await and onCreated — the completion is
    // dropped, not the request (the create landed and is journaled).
    const awaited = src.indexOf("await client.createDesk(");
    const created = src.indexOf("onCreated(created)");
    expect(awaited).toBeGreaterThan(-1);
    expect(guard).toBeGreaterThan(awaited);
    expect(created).toBeGreaterThan(guard);
  });

  it("replaces the fallback desk set with the first real channel instead of appending beside it", () => {
    const src = chatView();
    // loadDesks marks when the rail is showing defaultDesks() rather than the
    // host's own list. Since desk fabrication stopped standing in for an empty
    // answer, that is the 404 leg alone — a host with no `/desks` route at all.
    // An answered read is the company's own list, empty or not, and is never
    // the fallback set.
    expect(src).toContain("desksAreFallback.current = false;");
    expect(src).toContain("desksAreFallback.current = true;");
    expect(src).not.toContain("desksAreFallback.current = dtos.length === 0;");
    // …and onCreated replaces that set outright: the first real channel ends
    // the fallback's mandate, and appending beside it would keep fabricated
    // rows — one of which could share the new channel's id — until reload.
    expect(src).toContain(
      "setDesks((prev) => (desksAreFallback.current ? [desk] : [...(prev ?? []), desk]));",
    );
  });
});
