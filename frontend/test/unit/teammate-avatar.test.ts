import { existsSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, resolve } from "node:path";

import { describe, expect, it } from "vitest";

import { staticAvatarSrc } from "@/lib/avatar";
import { avatarFor } from "@/lib/team";

const here = dirname(fileURLToPath(import.meta.url));
const publicDir = resolve(here, "../../public");

describe("avatarFor", () => {
  it("gives the same teammate the same face every time", () => {
    // The whole reason this is a hash and not a random draw: nothing is
    // persisted, so a reload must not reshuffle the roster's faces.
    const first = avatarFor("agent_research_lead");
    for (let i = 0; i < 50; i++) {
      expect(avatarFor("agent_research_lead")).toBe(first);
    }
  });

  it("does not collapse a roster onto one face", () => {
    const seeds = Array.from({ length: 60 }, (_, i) => `teammate_${i}`);
    const distinct = new Set(seeds.map(avatarFor));
    // Not asserting a perfect spread — a hash makes no such promise. Asserting
    // that it spreads at all, which is what a one-line regression in the
    // modulus would break.
    expect(distinct.size).toBeGreaterThan(5);
  });

  it("seeds on the id, so renaming a teammate keeps its face", () => {
    // The model calls `avatarFor(dto.id || name)` for exactly this reason.
    expect(avatarFor("agent_maya")).toBe(avatarFor("agent_maya"));
    expect(avatarFor("agent_maya")).not.toBe(avatarFor("agent_maya_renamed"));
  });

  it("every key it can return has a file behind it", () => {
    // The failure this catches is silent in the browser: a key with no asset
    // renders as a broken image, and only on the teammates unlucky enough to
    // hash onto it.
    const reachable = new Set(
      Array.from({ length: 500 }, (_, i) => avatarFor(`seed_${i}`)),
    );
    expect(reachable.size).toBeGreaterThan(1);
    for (const ref of reachable) {
      const src = staticAvatarSrc(ref);
      expect(src, `${ref} resolves to no source at all`).not.toBeNull();
      const rel = src!.replace(/^\//, "");
      expect(existsSync(resolve(publicDir, rel)), `missing asset for ${ref}`).toBe(true);
    }
  });
});
