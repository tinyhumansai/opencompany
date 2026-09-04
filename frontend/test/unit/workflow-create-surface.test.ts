import { describe, expect, it } from "vitest";

import { ApiError } from "@/api/types";
import {
  createSurface,
  draftCapabilityGap,
  nameFromDescription,
} from "@/lib/workflow-create-surface";

/**
 * Which of the two New-workflow dialogs renders.
 *
 * This is the branch the redesign lives or dies on, and its failure is silent
 * in one direction: answer `form` on a create and the dialog looks exactly as
 * it did before, so nothing reports that the one-box dialog never shipped. A
 * rendered test can only prove the cases somebody thought to render; the
 * decision is a pure function so every input can be.
 *
 * There are only two inputs left, and that IS the change. The copilot's
 * availability used to be a third — an `echo` company and a build that answered
 * a capability gap both got the manual graph form. Running it settled it: a
 * host with no model showed the operator the full form, in the one case where
 * hand-authoring a graph is least likely to be what they wanted. What the
 * copilot can do now changes what Create *does*, never what the dialog *is*,
 * and that is asserted where it is visible — against the rendered dialog in
 * `workflow-one-box-dialog.test.ts`, with an `echo` company.
 */

describe("createSurface", () => {
  it("is the one box on every create, whatever the copilot can do", () => {
    expect(createSurface({ editing: false, writeRefused: false })).toBe("describe");
  });

  it("is the manual form in edit mode — an edit already has a graph", () => {
    expect(createSurface({ editing: true, writeRefused: false })).toBe("form");
  });

  it("is the manual form once a one-box create has been refused", () => {
    // The refusal that actually happens: the host mints a draft's id by
    // slugging and deduping against SAVED workflows only, so two similar
    // descriptions drafted before either is created mint the same id and the
    // second Create is told to pick a different one — by a dialog with no id
    // field. The fields have to come back or that is a dead end.
    expect(createSurface({ editing: false, writeRefused: true })).toBe("form");
  });

  it("answers every combination of its two inputs, so none is left to inference", () => {
    // Four rows is the whole truth table. Spelled out rather than looped,
    // because the one that matters is the first: a plain create, on any company
    // and any build, is the box.
    const table: [boolean, boolean, "describe" | "form"][] = [
      [false, false, "describe"],
      [false, true, "form"],
      [true, false, "form"],
      [true, true, "form"],
    ];
    for (const [editing, writeRefused, expected] of table) {
      expect(
        createSurface({ editing, writeRefused }),
        `editing=${editing} writeRefused=${writeRefused}`,
      ).toBe(expected);
    }
  });
});

describe("draftCapabilityGap", () => {
  it("names the three codes that mean this build cannot draft at all", () => {
    const cases: [number, string][] = [
      [404, "not_wired"],
      [409, "inference_required"],
      [409, "restart_required"],
    ];
    for (const [status, code] of cases) {
      expect(
        draftCapabilityGap(new ApiError(status, code, `refused: ${code}`)),
        `${status} ${code} is a capability gap`,
      ).toBe(`refused: ${code}`);
    }
  });

  it("does not treat an ordinary failure as a missing copilot", () => {
    // A dropped connection or a 500 says nothing about whether this company can
    // draft. Collapsing the redesign back to the old form over a flaky network
    // would be a redesign undone by wifi.
    expect(draftCapabilityGap(new ApiError(500, "internal", "boom"))).toBeNull();
    expect(draftCapabilityGap(new ApiError(400, "invalid_request", "describe it"))).toBeNull();
    expect(draftCapabilityGap(new Error("network down"))).toBeNull();
    expect(draftCapabilityGap("not_wired")).toBeNull();
    expect(draftCapabilityGap(null)).toBeNull();
  });

  it("keys on the code, never on the prose", () => {
    // A host that rewords its message must not silently change which dialog an
    // operator sees.
    expect(
      draftCapabilityGap(new ApiError(404, "unknown_route", "no copilot is wired here")),
    ).toBeNull();
  });
});

describe("nameFromDescription", () => {
  it("takes the first clause, which is where a sentence says what the thing is", () => {
    expect(
      nameFromDescription(
        "Every Monday morning, have the writer draft the digest and email it to the team.",
      ),
    ).toBe("Every Monday morning");
    expect(nameFromDescription("Chase overdue invoices. Weekly.")).toBe(
      "Chase overdue invoices",
    );
    expect(nameFromDescription("Publish the changelog")).toBe("Publish the changelog");
  });

  it("collapses whitespace and capitalises, so the name reads like a title", () => {
    expect(nameFromDescription("  weekly   digest\t  ")).toBe("Weekly digest");
  });

  it("caps a rambling clause rather than minting a paragraph-long name", () => {
    const long = "a".repeat(200);
    const name = nameFromDescription(long);
    expect(name.length).toBeLessThanOrEqual(61);
    expect(name.endsWith("…")).toBe(true);
  });

  it("derives nothing from a sentence with nothing usable in it", () => {
    // The caller must ASK for a name here rather than write an empty one: an
    // empty name derives an empty id, and the id is the permanent join key
    // nothing can fix after creation.
    expect(nameFromDescription("")).toBe("");
    expect(nameFromDescription("   ")).toBe("");
    expect(nameFromDescription(",,,")).toBe("");
    expect(nameFromDescription("...")).toBe("");
  });
});
