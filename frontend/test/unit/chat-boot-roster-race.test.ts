import { readFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import { describe, expect, it } from "vitest";

/**
 * A `boot()` roster read that resolves after a newer one must be discarded:
 * the shell bumps `rosterEpoch` (and re-fires `boot()`) on every roster
 * change, but the shell only serializes ITS OWN polling reads — nothing
 * serialized two overlapping `boot()` calls in `ChatView` against each
 * other. Two roster changes close together (a hire immediately followed by a
 * removal) could let the FIRST call's `listTeam` resolve after the SECOND's
 * and commit a stale roster last, silently reverting a fresher one this same
 * effect had already applied (CodeRabbit + Codex review, PR #2054: "Guard
 * boot() against stale roster responses" / "Discard stale roster refresh
 * responses").
 *
 * A jsdom render of `ChatView` cannot exercise this race — it needs the
 * whole client and every hook, and the failure is timing-shaped. So this
 * guards the wiring contract the fix rests on, the same source-contract
 * idiom `chat-mention-directory-race.test.ts` uses for the sibling
 * mention-directory race: a monotonic run token guards every commit path out
 * of `boot()`.
 */

const here = dirname(fileURLToPath(import.meta.url));
const read = (rel: string) => readFileSync(resolve(here, "../../src", rel), "utf8");

describe("ChatView boot() discards stale roster responses", () => {
  const chatView = read("views/ChatView.tsx");

  it("keeps a run token next to the boot() it guards", () => {
    expect(chatView).toContain("const bootRun = useRef(0);");
  });

  it("captures a fresh run before the roster read", () => {
    const boot = chatView.indexOf("const boot = useCallback");
    expect(boot).toBeGreaterThan(-1);
    expect(chatView.slice(boot, boot + 200)).toContain("const run = ++bootRun.current;");
  });

  it("guards the success path, the failure path, and the loading flag", () => {
    const boot = chatView.indexOf("const boot = useCallback");
    const bootEnd = chatView.indexOf("}, [client, company]);", boot);
    expect(bootEnd).toBeGreaterThan(boot);
    const body = chatView.slice(boot, bootEnd);

    // Success: a superseded resolution must not commit setMembers/setFromHost.
    expect(body).toContain("if (run !== bootRun.current) return;");
    // Failure: a superseded rejection must not clear a fresher roster either.
    const catchAt = body.indexOf("} catch {");
    expect(catchAt).toBeGreaterThan(-1);
    expect(body.slice(catchAt, catchAt + 100)).toContain("if (run !== bootRun.current) return;");
    // The loading flag is only ever cleared by the run that is still current.
    expect(body).toContain("if (run === bootRun.current) setLoadingTeam(false);");
  });
});
