import { describe, expect, it } from "vitest";

import { GATED_TOOLS } from "@/lib/gated-tools.generated";
import { UNNAMED_TOOL_ACTION, toolAction } from "@/lib/language";

/**
 * The TypeScript half of the label seam (issue #706).
 *
 * A tool the runtime classifies as `Reach::Consequence` parks for approval
 * under the default `supervised` mode, so an operator is shown a card for it
 * and has to decide. The words on that card come from `TOOL_LABELS` /
 * `EFFECT_LABELS` in `language.ts`, and until now nothing connected the two
 * sides: the declarations are Rust, the labels are TypeScript, and no build
 * step compared them. `workspace_create` sat rendering the generic
 * "Use one of its tools" from issue #551 onward with every lane green.
 *
 * `GATED_TOOLS` is generated from the Rust table
 * (`cargo test -- --ignored regenerate_gated_tools`) and a Rust test fails if
 * the committed copy drifts from it. So the two checks compose: Rust proves
 * the list is current, and this proves the list is named. Adding a
 * `Reach::Consequence` tool without a label now fails here, and changing the
 * table without regenerating fails there.
 *
 * Why assert through `toolAction` rather than against the label maps: those
 * maps are module-private, and the thing that actually matters is not "a key
 * exists" but "an operator sees words". Routing through the real resolver also
 * covers the `EFFECT_LABELS`-first ordering, which is how `composio_authorize`
 * and `mcp_registry_tool_call` are legitimately named without appearing in
 * `TOOL_LABELS` at all.
 */
describe("every gated tool has operator-facing words", () => {
  it("resolves each declared Consequence tool to a real label", () => {
    const unnamed = GATED_TOOLS.filter((tool) => toolAction(tool) === UNNAMED_TOOL_ACTION);
    expect(
      unnamed,
      `these tools park for approval with no plain-language label, so the card \
reads "${UNNAMED_TOOL_ACTION}" and an operator is asked to approve something \
unnamed: ${unnamed.join(", ")}. Add each to TOOL_LABELS in language.ts.`,
    ).toEqual([]);
  });

  /**
   * Fails closed. A check whose input silently arrives empty passes having
   * compared nothing, which is the same shape as the defect above: green, and
   * checking no one. The generated module is the input here, so its emptiness
   * is the failure mode worth pinning.
   */
  it("is not vacuous: the generated list is populated", () => {
    expect(GATED_TOOLS.length).toBeGreaterThan(10);
    // Named anchors, so a regeneration that emitted some unrelated remainder
    // could not leave the assertion above passing on a list that no longer
    // contains the consequential tools.
    for (const anchor of ["publish_artifact", "run_workflow", "shell"]) {
      expect(GATED_TOOLS).toContain(anchor);
    }
  });

  /**
   * The labels are what an operator reads in the Standing permissions list
   * (#374), where there is no payload block underneath to disambiguate them.
   * Two tools sharing a sentence are two permissions that cannot be told
   * apart, so distinctness is part of the contract rather than a nicety.
   */
  it("gives no two gated tools the same sentence", () => {
    const byLabel = new Map<string, string[]>();
    for (const tool of GATED_TOOLS) {
      const label = toolAction(tool);
      byLabel.set(label, [...(byLabel.get(label) ?? []), tool]);
    }
    const collisions = [...byLabel.entries()].filter(([, tools]) => tools.length > 1);
    expect(collisions, `tools sharing one label: ${JSON.stringify(collisions)}`).toEqual([]);
  });
});
