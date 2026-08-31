import { describe, expect, it } from "vitest";

import {
  destinationCheckDeferred,
  destinationTargetProblem,
} from "@/views/WorkflowCreateDialog";
import type { WiredChannels } from "@/views/WorkflowCreateDialog";

const ready = (...ids: string[]): WiredChannels => ({ status: "ready", ids });
const loading: WiredChannels = { status: "loading" };
const unavailable: WiredChannels = { status: "unavailable" };

// #813 defect 4: a channel destination pointing at a channel this deployment
// never wired only failed at delivery (`ChannelNotWired`). The picker keeps a
// create-mode author on the wired list, but the branch is still reachable — an
// edit dialog can carry a persisted target for a channel since unwired, and a
// free-text value can be entered when the host offered no list at all. These
// pin the author-time refusal on that path.
describe("destinationTargetProblem — unwired channel", () => {
  // `operator` is deliberately NOT in this fixture: this suite is about a
  // target absent from the wired set, and operator's own presence is covered
  // separately below.
  const wired = ready("engineering", "product_design");

  it("rejects a channel that is not in the wired set, naming what is", () => {
    const problem = destinationTargetProblem("channel", "ghost", wired);
    expect(problem).toBe(
      "`ghost` is not a workflow delivery channel — this runtime has: engineering, product_design.",
    );
  });

  it("rejects `operator` when the host's wired set doesn't include it", () => {
    expect(destinationTargetProblem("channel", "operator", wired)).toContain(
      "is not a workflow delivery channel",
    );
  });

  // #1757: `operator` used to be excluded from every host's wired set (#981) —
  // an in-memory surface delivery refused by name. It is now a durable,
  // journal-backed channel every company wires, so the function that gates a
  // channel target on membership in `channels.ids` accepts it exactly like any
  // other name the host lists — no special-casing either way.
  it("accepts `operator` once the host's wired set includes it", () => {
    expect(
      destinationTargetProblem("channel", "operator", ready("operator", "engineering")),
    ).toBeNull();
  });

  it("accepts a channel that is wired", () => {
    expect(destinationTargetProblem("channel", "engineering", wired)).toBeNull();
    expect(destinationTargetProblem("channel", "product_design", wired)).toBeNull();
  });

  // #981: a settled empty answer is knowledge, not ignorance — the company has
  // no desks and no connected channels, and the host refuses every channel
  // target against that same empty set. The pre-flight has to agree, in the
  // host's own words for the empty case (`undeliverable_channel_message`).
  it("rejects any channel when the host answered with an empty set", () => {
    expect(destinationTargetProblem("channel", "engineering", ready())).toBe(
      "`engineering` is not a workflow delivery channel — this runtime has: no durable channels.",
    );
  });

  it("does not reject free text when the host could not answer", () => {
    // A failed request / a host predating the route tells us nothing, so a
    // free-text box must not be wrongly refused — the save-time 400 is the gate.
    expect(destinationTargetProblem("channel", "anything", unavailable)).toBeNull();
  });
});

// #981: the check used to be skipped while the list was loading, and a skip is
// indistinguishable from a pass. An author who clicked Save before the fetch
// settled got no channel pre-flight at all; a slower author got the full one.
describe("destinationCheckDeferred — the loading-order trap", () => {
  it("does not silently pass a channel target while the list is loading", () => {
    // The pair is the point: the target check alone cannot answer (`null`), and
    // the deferral is what stops that `null` from being read as "fine".
    expect(destinationTargetProblem("channel", "ghost", loading)).toBeNull();
    expect(destinationCheckDeferred("channel", "ghost", loading)).toBe(
      "still checking which channels this company can deliver to — try Save again in a moment.",
    );
  });

  it("stops deferring the moment the answer lands", () => {
    expect(destinationCheckDeferred("channel", "ghost", ready("engineering"))).toBeNull();
    expect(destinationCheckDeferred("channel", "ghost", ready())).toBeNull();
  });

  it("never defers when the host cannot answer at all", () => {
    // Otherwise a host without the route would block Save forever.
    expect(destinationCheckDeferred("channel", "ghost", unavailable)).toBeNull();
  });

  it("only defers what it is about: a channel target that exists", () => {
    expect(destinationCheckDeferred("channel", "", loading)).toBeNull();
    expect(destinationCheckDeferred("channel", "   ", loading)).toBeNull();
    expect(destinationCheckDeferred("email", "ada@example.com", loading)).toBeNull();
    expect(destinationCheckDeferred("owner", "", loading)).toBeNull();
    expect(destinationCheckDeferred("", "", loading)).toBeNull();
  });
});
