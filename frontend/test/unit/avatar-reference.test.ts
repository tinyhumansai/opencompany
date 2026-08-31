// The avatar reference grammar, and the two rules it exists to keep.
//
// The grammar is mirrored in two places — `src/company/avatar.rs` validates it
// and `src/lib/avatar.ts` renders it — so the first test here reads the Rust
// source and asserts the flavour lists match. A flavour one side accepts and the
// other has no file for renders as a broken image on every surface at once, and
// nothing else in the build would notice.

import { readFileSync, existsSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, resolve } from "node:path";

import { describe, expect, it } from "vitest";

import {
  MAX_AVATAR_MB,
  TINY_FLAVOURS,
  avatarRef,
  blobNodeId,
  hashedFlavour,
  staticAvatarSrc,
  tinySrc,
} from "@/lib/avatar";

const here = dirname(fileURLToPath(import.meta.url));
const repoRoot = resolve(here, "../../..");

describe("the tiny flavours", () => {
  it("are the same list the host validates against", () => {
    const rust = readFileSync(resolve(repoRoot, "src/company/avatar.rs"), "utf8");
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

describe("the upload ceiling", () => {
  it("is the same number the host enforces", () => {
    // The picker prints this before anybody picks a file; the host is what
    // actually refuses. Two different figures would mean the copy promises a
    // size the upload then rejects.
    const rust = readFileSync(resolve(repoRoot, "src/company/avatar.rs"), "utf8");
    const bytes = /MAX_AVATAR_BYTES: usize = (\d+) \* 1024 \* 1024/.exec(rust);
    expect(bytes, "MAX_AVATAR_BYTES is no longer declared the way this test reads it").not.toBeNull();
    expect(Number(bytes![1])).toBe(MAX_AVATAR_MB);
  });
});

describe("staticAvatarSrc", () => {
  it("resolves a mascot without a fetch", () => {
    expect(staticAvatarSrc("tiny:teal")).toBe("/avatars/blob-teal.webp");
  });

  it("draws nothing for anything that is not one of the two forms", () => {
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

  it("does not resolve an upload synchronously — that one needs the client", () => {
    expect(staticAvatarSrc("blob:01J8Z5Q9YQ")).toBeNull();
    expect(blobNodeId("blob:01J8Z5Q9YQ")).toBe("01J8Z5Q9YQ");
    expect(blobNodeId("tiny:teal")).toBeNull();
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

  it("keeps a teammate's default face stable across reloads", () => {
    // The whole reason the default is a hash and not a draw: nothing is stored
    // for it, so it has to be recomputable to the same answer forever.
    for (let i = 0; i < 20; i++) {
      expect(hashedFlavour("agent_research_lead")).toBe(hashedFlavour("agent_research_lead"));
    }
  });
});
