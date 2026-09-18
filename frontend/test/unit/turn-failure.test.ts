// The fail-closed turn notice (keys rework, issue #2306, round-2 review
// KR-L2-03): the host's exact X9 sentence, rendered verbatim, with the one
// action that fixes it. Coded against the orchestrator's stated contract
// ahead of `docs/key-reworks/in-use-guards.md` §5 documenting it for real —
// see `src/lib/turn-failure.ts`'s own module doc.

import { describe, expect, it } from "vitest";

import {
  TURN_FAILURE_CODES,
  toTurnFailure,
  turnFailureAction,
  type TurnFailureWire,
} from "@/lib/turn-failure";

function wire(over: Partial<TurnFailureWire> = {}): TurnFailureWire {
  return {
    userFacing: true,
    code: "no_model_chosen",
    message: "No model is chosen. Choose a provider and model for Writer, or set the company default in Connections → API Keys → LLM.",
    ...over,
  };
}

describe("toTurnFailure", () => {
  it("builds a failure from a well-formed wire payload", () => {
    expect(toTurnFailure(wire())).toEqual({
      userFacing: true,
      code: "no_model_chosen",
      message: wire().message,
      pairAgentId: undefined,
      providerSlug: undefined,
    });
  });

  it("carries pairAgentId and providerSlug through when present", () => {
    const failure = toTurnFailure(wire({ pairAgentId: "writer", providerSlug: "acme" }));
    expect(failure?.pairAgentId).toBe("writer");
    expect(failure?.providerSlug).toBe("acme");
  });

  it("is undefined when userFacing is absent or false", () => {
    expect(toTurnFailure(wire({ userFacing: undefined }))).toBeUndefined();
    expect(toTurnFailure(wire({ userFacing: false }))).toBeUndefined();
  });

  it("is undefined with no code or no message, even when userFacing is true", () => {
    expect(toTurnFailure(wire({ code: undefined }))).toBeUndefined();
    expect(toTurnFailure(wire({ code: "" }))).toBeUndefined();
    expect(toTurnFailure(wire({ message: undefined }))).toBeUndefined();
    expect(toTurnFailure(wire({ message: "  " }))).toBeUndefined();
  });

  it("is undefined for null/undefined input, and for every other failure a reply carries (a provider outage, a rate limit) with no structured reason at all", () => {
    expect(toTurnFailure(null)).toBeUndefined();
    expect(toTurnFailure(undefined)).toBeUndefined();
    expect(toTurnFailure({})).toBeUndefined();
  });
});

describe("a bound-harness failure's sentence", () => {
  const message =
    "agent `researcher` is bound to harness `claude-code`, but it is an ACP harness and " +
    "this build has no ACP transport wired. Retrying will not help until that harness can run.";

  it("reaches the notice with the harness and the reason intact", () => {
    const failure = toTurnFailure(wire({ code: "harness_unavailable", message, pairAgentId: "researcher" }));
    expect(failure?.message).toContain("claude-code");
    expect(failure?.message).toContain("no ACP transport wired");
    expect(failure?.pairAgentId).toBe("researcher");
  });
});

describe("turnFailureAction — one test per code the dispatch named", () => {
  it("no_model_chosen opens LLM settings", () => {
    const action = turnFailureAction({ code: "no_model_chosen" });
    expect(action).toEqual({ label: "Open LLM settings", href: "#/connections/inference" });
  });

  it("pair_provider_removed opens that agent's Model tab", () => {
    const action = turnFailureAction({ code: "pair_provider_removed", pairAgentId: "page_builder" });
    expect(action).toEqual({ label: "Open Model settings", href: "#/company/agent/page_builder?tab=model" });
  });

  it("pair_provider_off opens that agent's Model tab", () => {
    const action = turnFailureAction({ code: "pair_provider_off", pairAgentId: "researcher" });
    expect(action).toEqual({ label: "Open Model settings", href: "#/company/agent/researcher?tab=model" });
  });

  it("default_provider_removed opens LLM settings", () => {
    const action = turnFailureAction({ code: "default_provider_removed" });
    expect(action).toEqual({ label: "Open LLM settings", href: "#/connections/inference" });
  });

  it("default_provider_off opens LLM settings", () => {
    const action = turnFailureAction({ code: "default_provider_off" });
    expect(action).toEqual({ label: "Open LLM settings", href: "#/connections/inference" });
  });

  it("provider_no_key opens LLM settings", () => {
    const action = turnFailureAction({ code: "provider_no_key", pairAgentId: "writer" });
    expect(action).toEqual({ label: "Open LLM settings", href: "#/connections/inference" });
  });

  it("model_not_listed opens LLM settings", () => {
    const action = turnFailureAction({ code: "model_not_listed" });
    expect(action).toEqual({ label: "Open LLM settings", href: "#/connections/inference" });
  });

  // The turn never reached a model at all: the teammate is bound to a harness
  // this host cannot run (issue #2394). The LLM page has no control that fixes
  // that — the harness binding lives on the teammate's own Model tab — so this
  // code shares the pair bucket despite not being a `pair_` code.
  it("harness_unavailable opens that agent's Model tab, not LLM settings", () => {
    const action = turnFailureAction({ code: "harness_unavailable", pairAgentId: "researcher" });
    expect(action).toEqual({ label: "Open Model settings", href: "#/company/agent/researcher?tab=model" });
  });

  it("harness_unavailable with no pairAgentId has nothing to link, so no button", () => {
    expect(turnFailureAction({ code: "harness_unavailable" })).toBeNull();
  });

  it("every named code is covered by exactly this test suite", () => {
    // A guard against the list growing silently uncovered — fails loudly if
    // a code is added to TURN_FAILURE_CODES without a matching case above.
    expect(TURN_FAILURE_CODES).toEqual([
      "no_model_chosen",
      "pair_provider_removed",
      "pair_provider_off",
      "default_provider_removed",
      "default_provider_off",
      "provider_no_key",
      "model_not_listed",
      "harness_unavailable",
    ]);
  });

  it("a pair code with no pairAgentId has nothing useful to link, so no button", () => {
    expect(turnFailureAction({ code: "pair_provider_removed" })).toBeNull();
  });

  it("an unrecognised code from a newer host still gets the safe default action", () => {
    const action = turnFailureAction({ code: "some_future_code" });
    expect(action).toEqual({ label: "Open LLM settings", href: "#/connections/inference" });
  });
});
