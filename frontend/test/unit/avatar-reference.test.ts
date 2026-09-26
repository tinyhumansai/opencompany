// The avatar reference grammar, and the two rules it exists to keep.
//
// The grammar is mirrored in two places — `crates/opencompany-core/src/company/avatar.rs` validates it
// and `src/lib/avatar.ts` renders it — so the first test here reads the Rust
// source and asserts the flavour lists match. A flavour one side accepts and the
// other has no file for renders as a broken image on every surface at once, and
// nothing else in the build would notice.

import { readFileSync, existsSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, resolve } from "node:path";

import { describe, expect, it } from "vitest";

import {
  MASCOT_COSTUMES,
  MASCOT_HAND_COLORS,
  MASCOT_KINDS,
  MASCOT_MODES,
  MASCOT_SKIN_COLORS,
  MAX_AVATAR_MB,
  TINY_FLAVOURS,
  avatarRef,
  blobNodeId,
  hashedFlavour,
  isMascotRef,
  mascotSrc,
  staticAvatarSrc,
  tinySrc,
} from "@/lib/avatar";

const here = dirname(fileURLToPath(import.meta.url));
const repoRoot = resolve(here, "../../..");

describe("the tiny flavours", () => {
  it("are the same list the host validates against", () => {
    const rust = readFileSync(resolve(repoRoot, "crates/opencompany-core/src/company/avatar.rs"), "utf8");
    const block = /pub const TINY_FLAVOURS: \[&str; \d+\] = \[([^\]]*)\]/s.exec(rust);
    expect(block, "TINY_FLAVOURS is no longer declared the way this test reads it").not.toBeNull();
    const hostFlavours = Array.from(block![1].matchAll(/"([a-z]+)"/g), (m) => m[1]);
    expect([...hostFlavours].sort()).toEqual([...TINY_FLAVOURS].sort());
  });

  it("each have a file behind them", () => {
    for (const flavour of TINY_FLAVOURS) {
      const rel = tinySrc(flavour).replace(/^\//, "");
      expect(existsSync(resolve(repoRoot, "frontend/public", rel)), flavour).toBe(true);
    }
  });
});

describe("the mascot kinds", () => {
  it("are the same list the host validates against", () => {
    const rust = readFileSync(resolve(repoRoot, "crates/opencompany-core/src/company/avatar.rs"), "utf8");
    const block = /pub const MASCOT_KINDS: \[&str; \d+\] = \[([^\]]*)\]/s.exec(rust);
    expect(block, "MASCOT_KINDS is no longer declared the way this test reads it").not.toBeNull();
    const hostKinds = Array.from(block![1].matchAll(/"([a-z]+)"/g), (m) => m[1]);
    expect([...hostKinds].sort()).toEqual([...MASCOT_KINDS].sort());
  });

  it("each have a file behind them", () => {
    for (const kind of MASCOT_KINDS) {
      const rel = mascotSrc(kind).replace(/^\//, "");
      expect(existsSync(resolve(repoRoot, "frontend/public", rel)), kind).toBe(true);
    }
  });
});

const mascotRustSource = () =>
  readFileSync(resolve(repoRoot, "crates/opencompany-core/src/company/mascot.rs"), "utf8");

describe("the mascot display modes", () => {
  it("are the same list the host validates against", () => {
    const rust = mascotRustSource();
    const block = /pub const MASCOT_MODES: \[&str; \d+\] = \[([^\]]*)\]/s.exec(rust);
    expect(block, "MASCOT_MODES is no longer declared the way this test reads it").not.toBeNull();
    const hostModes = Array.from(block![1].matchAll(/"([a-z]+)"/g), (m) => m[1]);
    expect([...hostModes].sort()).toEqual([...MASCOT_MODES].sort());
  });

  it("animated is the file's own default, first in the list", () => {
    expect(MASCOT_MODES[0]).toBe("animated");
  });
});

describe("the mascot costumes", () => {
  it("are the same ids and numbers the host validates against", () => {
    const rust = mascotRustSource();
    const idsBlock = /pub const MASCOT_COSTUMES: \[&str; \d+\] = \[([\s\S]*?)\n\];/.exec(rust);
    expect(idsBlock, "MASCOT_COSTUMES is no longer declared the way this test reads it").not.toBeNull();
    const hostIds = Array.from(idsBlock![1].matchAll(/"([a-z0-9_]+)"/g), (m) => m[1]);
    expect([...hostIds].sort()).toEqual([...MASCOT_COSTUMES.map((c) => c.id)].sort());

    const numbersBlock = /pub const MASCOT_COSTUME_NUMBERS: \[u8; \d+\] = \[([^\]]*)\]/s.exec(rust);
    expect(
      numbersBlock,
      "MASCOT_COSTUME_NUMBERS is no longer declared the way this test reads it",
    ).not.toBeNull();
    const hostNumbers = Array.from(numbersBlock![1].matchAll(/(\d+)/g), (m) => Number(m[1]));
    // Paired 1:1 with MASCOT_COSTUMES by position on both sides — not just the
    // same set, the same costume gets the same number, or a picker choice
    // would render a different costume than the one it named.
    expect(hostIds.map((id, i) => [id, hostNumbers[i]])).toEqual(
      MASCOT_COSTUMES.map((c) => [c.id, c.number]),
    );
  });

  it("cap is the file's own default, first in the list", () => {
    expect(MASCOT_COSTUMES[0]).toMatchObject({ id: "cap", number: 1 });
  });

  it("each have a file behind them (the one shipped .riv, shared by every costume)", () => {
    for (const { id } of MASCOT_COSTUMES) {
      expect(existsSync(resolve(repoRoot, "frontend/public/avatars/mascot-animated.riv")), id).toBe(
        true,
      );
    }
  });
});

describe("the mascot colors", () => {
  it("skin colors are the same ids and hexes the host validates against", () => {
    const rust = mascotRustSource();
    const idsBlock = /pub const MASCOT_SKIN_COLORS: \[&str; \d+\] = \[([^\]]*)\]/s.exec(rust);
    expect(idsBlock, "MASCOT_SKIN_COLORS is no longer declared the way this test reads it").not.toBeNull();
    const hostIds = Array.from(idsBlock![1].matchAll(/"([a-z]+)"/g), (m) => m[1]);
    expect([...hostIds].sort()).toEqual([...MASCOT_SKIN_COLORS.map((c) => c.id)].sort());

    // rustfmt wraps this one onto two lines (the array itself pushes the
    // declaration past its line-length limit), so `=` and `[` are separated
    // by a newline and indentation rather than the single space the shorter
    // consts above get — hence `=\s*\[` instead of ` = \[`.
    const hexBlock = /pub const MASCOT_SKIN_COLOR_HEXES: \[&str; \d+\] =\s*\[([^\]]*)\]/s.exec(rust);
    expect(hexBlock, "MASCOT_SKIN_COLOR_HEXES is no longer declared the way this test reads it").not.toBeNull();
    const hostHexes = Array.from(hexBlock![1].matchAll(/"([0-9a-f]{6})"/g), (m) => m[1]);
    expect(hostIds.map((id, i) => [id, hostHexes[i]])).toEqual(
      MASCOT_SKIN_COLORS.map((c) => [c.id, c.hex.replace("#", "").toLowerCase()]),
    );
  });

  it("hand colors are the same ids and hexes the host validates against", () => {
    const rust = mascotRustSource();
    const idsBlock = /pub const MASCOT_HAND_COLORS: \[&str; \d+\] = \[([^\]]*)\]/s.exec(rust);
    expect(idsBlock, "MASCOT_HAND_COLORS is no longer declared the way this test reads it").not.toBeNull();
    const hostIds = Array.from(idsBlock![1].matchAll(/"([a-z]+)"/g), (m) => m[1]);
    expect([...hostIds].sort()).toEqual([...MASCOT_HAND_COLORS.map((c) => c.id)].sort());

    // Same rustfmt line-wrap as MASCOT_SKIN_COLOR_HEXES above.
    const hexBlock = /pub const MASCOT_HAND_COLOR_HEXES: \[&str; \d+\] =\s*\[([^\]]*)\]/s.exec(rust);
    expect(hexBlock, "MASCOT_HAND_COLOR_HEXES is no longer declared the way this test reads it").not.toBeNull();
    const hostHexes = Array.from(hexBlock![1].matchAll(/"([0-9a-f]{6})"/g), (m) => m[1]);
    expect(hostIds.map((id, i) => [id, hostHexes[i]])).toEqual(
      MASCOT_HAND_COLORS.map((c) => [c.id, c.hex.replace("#", "").toLowerCase()]),
    );
  });

  it("both defaults are first and match the .riv file's own shipped colors", () => {
    expect(MASCOT_SKIN_COLORS[0]).toEqual({ id: "default", hex: "#F7D145" });
    expect(MASCOT_HAND_COLORS[0]).toEqual({ id: "default", hex: "#B4900B" });
  });
});

describe("the upload ceiling", () => {
  it("is the same number the host enforces", () => {
    // The picker prints this before anybody picks a file; the host is what
    // actually refuses. Two different figures would mean the copy promises a
    // size the upload then rejects.
    const rust = readFileSync(resolve(repoRoot, "crates/opencompany-core/src/company/avatar.rs"), "utf8");
    const bytes = /MAX_AVATAR_BYTES: usize = (\d+) \* 1024 \* 1024/.exec(rust);
    expect(bytes, "MAX_AVATAR_BYTES is no longer declared the way this test reads it").not.toBeNull();
    expect(Number(bytes![1])).toBe(MAX_AVATAR_MB);
  });
});

describe("staticAvatarSrc", () => {
  it("resolves a mascot without a fetch", () => {
    expect(staticAvatarSrc("tiny:teal")).toBe("/avatars/blob-teal.webp");
  });

  it("draws nothing for anything that is not one of the three forms", () => {
    // The rule the closed grammar exists for: this value ends up in an `src=`
    // on every surface that draws a face, so an unrecognised one — which can
    // only be version skew, since the host stores nothing else — must resolve to
    // no source at all rather than to itself.
    for (const hostile of [
      "https://tracker.example/beacon.gif",
      "javascript:alert(1)",
      "data:image/gif;base64,R0lGOD",
      "/avatars/blob-amber.webp",
      "amber",
      "",
    ]) {
      expect(staticAvatarSrc(hostile), hostile).toBeNull();
    }
  });

  it("resolves nothing for a mascot — it's a valid form, just not a static image", () => {
    // Not hostile input (unlike the table above): `mascot:animated` is a
    // legitimate, host-accepted reference. It deliberately draws nothing here
    // so every mass-render surface keeps falling back to the tone tile; only
    // the hero surfaces that explicitly mount `MascotAvatar` render it live.
    expect(staticAvatarSrc("mascot:animated")).toBeNull();
  });

  it("does not resolve an upload synchronously — that one needs the client", () => {
    expect(staticAvatarSrc("blob:01J8Z5Q9YQ")).toBeNull();
    expect(blobNodeId("blob:01J8Z5Q9YQ")).toBe("01J8Z5Q9YQ");
    expect(blobNodeId("tiny:teal")).toBeNull();
  });
});

describe("isMascotRef", () => {
  it("recognises a mascot reference regardless of surrounding whitespace", () => {
    expect(isMascotRef("mascot:animated")).toBe(true);
    expect(isMascotRef("  mascot:animated  ")).toBe(true);
  });

  it("does not match the other two forms, or nonsense", () => {
    for (const other of ["tiny:teal", "blob:01J8Z5Q9YQ", "", "mascotx:animated", "MASCOT:animated"]) {
      expect(isMascotRef(other), other).toBe(false);
    }
  });

  // `isMascotRef` only decides whether to mount the live canvas at all — it
  // does not validate `kind` against `MASCOT_KINDS`, the same way
  // `staticAvatarSrc`'s `tiny:` branch does not validate its flavour against
  // `TINY_FLAVOURS` either. Both defer that to the host's `AvatarRef::parse`
  // (`crates/opencompany-core/src/company/avatar.rs`), the actual
  // persistence boundary — an unrecognised kind can never be written in the
  // first place. `MascotAvatar` also never reads `kind` out of the stored
  // reference (v1 hardcodes the one shipped `.riv`), so an unvalidated kind
  // here has no path to a broken render even before the host's own check.
  it("matches any kind suffix — validation is the host's job, same as tiny:", () => {
    expect(isMascotRef("mascot:not-a-real-kind")).toBe(true);
  });
});

describe("avatarRef", () => {
  it("prefers the chosen face and falls back to the hashed one", () => {
    expect(avatarRef("tiny:rose", "agent_ada")).toBe("tiny:rose");
    expect(avatarRef(undefined, "agent_ada")).toBe(`tiny:${hashedFlavour("agent_ada")}`);
    // A blank choice is not a choice — the host stores nothing for it, and a
    // client that treated `""` as one would render an empty `src`.
    expect(avatarRef("   ", "agent_ada")).toBe(`tiny:${hashedFlavour("agent_ada")}`);
  });

  it("keeps an agent's default face stable across reloads", () => {
    // The whole reason the default is a hash and not a draw: nothing is stored
    // for it, so it has to be recomputable to the same answer forever.
    for (let i = 0; i < 20; i++) {
      expect(hashedFlavour("agent_research_lead")).toBe(hashedFlavour("agent_research_lead"));
    }
  });
});
