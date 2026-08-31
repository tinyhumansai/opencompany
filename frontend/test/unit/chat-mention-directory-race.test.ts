import { readFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import { describe, expect, it } from "vitest";

/**
 * A mention-directory reload that resolves after a newer fetch must be
 * discarded: a roster write just before a company switch would otherwise apply
 * the old company's rows over the new company's directory, and the picker
 * would advertise stale — possibly cross-company — targets until the next
 * reload (PR #1669 review: "discard stale mention-directory reloads").
 *
 * A jsdom render of `ChatView` cannot exercise this race — it needs the whole
 * client and every hook, and the failure is timing-shaped. So this guards the
 * wiring contract the fix rests on, the same source-contract idiom as
 * `chat-rail-focus.test.ts`: the mount fetch and `reloadDirectory` share one
 * epoch token, every fetch bumps it, and no `setDirectory` runs once the token
 * has moved on.
 */

const here = dirname(fileURLToPath(import.meta.url));
const read = (rel: string) => readFileSync(resolve(here, "../../src", rel), "utf8");

describe("mention-directory reloads discard stale completions", () => {
  const chatView = read("views/ChatView.tsx");

  it("shares one epoch token between the mount fetch and reloadDirectory", () => {
    // The token lives next to the directory it guards, not inside either
    // fetch site — both must bump and read the *same* counter.
    expect(chatView).toContain("const directoryEpoch = useRef(0);");
  });

  it("bumps the token before every fetch", () => {
    // Both the mount effect and the roster-write reload capture a fresh epoch
    // before their request, so a superseded in-flight response's captured
    // token no longer matches by the time it resolves.
    const reload = chatView.indexOf("const reloadDirectory = useCallback");
    expect(reload).toBeGreaterThan(-1);
    expect(chatView.slice(reload, reload + 500)).toContain(
      "const epoch = ++directoryEpoch.current;",
    );
    const effect = chatView.indexOf("useEffect(() => {", reload);
    expect(effect).toBeGreaterThan(-1);
    expect(chatView.slice(effect, effect + 500)).toContain(
      "const epoch = ++directoryEpoch.current;",
    );
  });

  it("guards every setDirectory with the epoch check", () => {
    // The unguarded `setDirectory` in the old `reloadDirectory` was exactly
    // the stale-write bug: it applied whatever resolved, even after a newer
    // directory. Both success and failure must wait for the token.
    const guards = chatView.match(
      /if \(epoch === directoryEpoch\.current\) setDirectory/g,
    );
    expect(guards?.length).toBe(4);
  });

  it("clears the directory synchronously when a company switch starts a fetch", () => {
    // The mount effect still nulls the directory before the request, so the
    // picker never shows the old company's rows while the new directory is on
    // the wire. Anchored after `reloadDirectory` to find *this* effect, not an
    // unrelated `useEffect` earlier in the view.
    const reload = chatView.indexOf("const reloadDirectory = useCallback");
    const reloadEnd = chatView.indexOf("}, [client, company]);", reload);
    const effect = chatView.indexOf("useEffect(() => {", reloadEnd);
    expect(effect).toBeGreaterThan(-1);
    expect(chatView.slice(effect, effect + 200)).toContain("setDirectory(null);");
  });
});
