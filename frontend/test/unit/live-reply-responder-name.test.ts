import { describe, expect, it } from "vitest";

import { mergeOpenTurns, openTurnsFromRuns, type OpenRunRow } from "@/lib/live-reply";

/**
 * The name the reload leg renders.
 *
 * A console that sent the turn itself holds a receipt, which names the teammate
 * off the first live frame. A console that reloaded holds neither the receipt
 * nor the frames — only the durable rows `/runs` hands back — and until the host
 * recorded a responder on those rows there was nothing on them to name. The
 * re-armed indicator could say only "Working…", which reads the same as a
 * console that has lost the turn entirely.
 *
 * Reproduced against a live company before it was written: a chat turn's row
 * carried `agentId === chatId === "main"`, the channel rather than a teammate,
 * so no amount of resolving could have produced a name.
 */
describe("carrying the answering teammate onto the reload leg", () => {
  const run = (over: Partial<OpenRunRow> = {}): OpenRunRow => ({
    id: "turn-1",
    chatId: "main",
    status: "running",
    agentId: "triage",
    ...over,
  });

  it("carries the responder the host recorded", () => {
    const open = openTurnsFromRuns([run()]);
    expect(open["main"]?.[0]?.agentId).toBe("triage");
  });

  it("keeps naming nobody when the host named nobody", () => {
    const open = openTurnsFromRuns([run({ agentId: undefined })]);
    expect(open["main"]?.[0]?.agentId).toBeUndefined();
  });

  /**
   * The regression the conditional spread in `openTurnsFromRuns` exists for.
   *
   * `mergeOpenTurns` folds a re-arm onto the row the POST leg already put in the
   * map by spreading the incoming row over it. A row carrying `agentId:
   * undefined` as a *present* key would erase a name that was already there —
   * the console would name the teammate, then un-name them the moment the runs
   * poll answered. Omitting the key leaves the standing name alone.
   */
  it("does not erase a known name when the re-arm carries none", () => {
    const armed = { "main": [{ turnId: "turn-1", queued: false, chatId: "main", agentId: "triage" }] };
    const reArmed = openTurnsFromRuns([run({ agentId: undefined })]);
    const merged = mergeOpenTurns(armed, reArmed);
    expect(merged["main"]?.[0]?.agentId).toBe("triage");
  });

  it("lets a later re-arm correct the name", () => {
    const armed = { "main": [{ turnId: "turn-1", queued: true, chatId: "main", agentId: "triage" }] };
    const merged = mergeOpenTurns(armed, openTurnsFromRuns([run({ agentId: "amendments" })]));
    expect(merged["main"]?.[0]?.agentId).toBe("amendments");
  });

  /**
   * A task dispatch carries no `chatId` — it is reachable through its card — and
   * is dropped by the fold outright. Pinned here because the row DOES carry an
   * `agentId`, so a fold that keyed on the agent rather than the chat would
   * happily file a dispatch under a channel nobody is watching.
   */
  it("still drops a row with no chat, however well named", () => {
    expect(openTurnsFromRuns([run({ chatId: undefined, agentId: "company_line" })])).toEqual({});
  });
});
