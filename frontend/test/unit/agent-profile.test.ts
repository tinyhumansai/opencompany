import { describe, expect, it } from "vitest";

import { agentHref, agentProfile } from "@/lib/agent-profile";
import type { AgentDetailDto } from "@/api/types";

/**
 * The derivations behind the profile panel a teammate's avatar opens (issue
 * #1653).
 *
 * Each is a place where being wrong renders as a perfectly ordinary panel: a
 * summary quoting a blueprint persona the agent is not running, "no tools" over
 * an agent that holds everything the company allows, or an Edit button that
 * lands on the read-only page.
 */

function agent(over: Partial<AgentDetailDto> = {}): AgentDetailDto {
  return {
    id: "jamie",
    name: "Jamie",
    role: "Growth",
    description: "Runs paid acquisition.",
    source: "overlay",
    editable: ["name", "role", "description"],
    isOrchestrator: false,
    tools: { requested: null, companyAllow: ["workspace.*"], deskAllow: [], deskCeilingActive: false, effective: ["workspace.*"] },
    desks: [],
    inboxEnabled: false,
    ...over,
  };
}

describe("what the panel says a teammate is", () => {
  it("shows the instructions in force, not the blueprint they mask", () => {
    const profile = agentProfile(
      agent({
        description: "Runs paid acquisition.",
        instructions: "Confirm the budget before launching anything.",
        blueprintInstructions: "Spend freely.",
        instructionsOverridden: true,
      }),
    );
    expect(profile.about).toBe("Confirm the budget before launching anything.");
  });

  it("falls back to the description for a teammate with no instructions", () => {
    expect(agentProfile(agent({ instructions: null })).about).toBe("Runs paid acquisition.");
  });

  it("has nothing to say about a teammate defined with neither", () => {
    const profile = agentProfile(agent({ description: undefined, instructions: null }));
    expect(profile.about).toBeNull();
    expect(profile.aboutTruncated).toBe(false);
  });

  it("clips a long persona and admits that it did", () => {
    const profile = agentProfile(agent({ instructions: `${"word ".repeat(200)}end` }));
    expect(profile.aboutTruncated).toBe(true);
    expect(profile.about?.endsWith("…")).toBe(true);
    expect(profile.about!.length).toBeLessThanOrEqual(321);
  });

  it("leaves a persona that fits exactly as written", () => {
    const short = "Answers the front desk.";
    const profile = agentProfile(agent({ instructions: short }));
    expect(profile.about).toBe(short);
    expect(profile.aboutTruncated).toBe(false);
  });

  it("shows a manifest teammate by its role, and says so once", () => {
    const profile = agentProfile(
      agent({ name: undefined, role: "Chief Executive", source: "manifest" }),
    );
    expect(profile.display).toBe("Chief Executive");
    // The role would only repeat the title, so the subtitle is dropped rather
    // than rendered as the same words twice (issue #1208).
    expect(profile.subtitle).toBeNull();
    expect(profile.origin).toBe("Company blueprint");
  });

  it("reads the tier from the resolved orchestrator, not the tier string", () => {
    expect(agentProfile(agent({ tier: "worker", isOrchestrator: true })).tier).toBe("Orchestrator");
  });

  it("seeds the face on the id, so a rename keeps the same avatar", () => {
    const before = agentProfile(agent());
    const after = agentProfile(agent({ name: "Jay" }));
    expect(after.avatar).toBe(before.avatar);
    expect(after.tone).toBe(before.tone);
  });

  it("keeps an absent tool request (null) legible as the standard grant", () => {
    const profile = agentProfile(agent());
    expect(profile.tools.standardGrant).toBe(true);
    expect(profile.tools.deniedAll).toBe(false);
    expect(profile.tools.effective).toEqual(["workspace.*"]);
  });

  it("names the globs an agent asked for that the company does not allow", () => {
    const profile = agentProfile(
      agent({
        tools: {
          requested: ["workspace.*", "finance.*"],
          companyAllow: ["workspace.*"],
          deskAllow: [],
          deskCeilingActive: false,
          effective: ["workspace.*"],
        },
      }),
    );
    expect(profile.tools.standardGrant).toBe(false);
    expect(profile.tools.dropped).toEqual(["finance.*"]);
  });
});

describe("where the panel's buttons go", () => {
  it("links to the teammate's page", () => {
    expect(agentHref("jamie")).toBe("#/team/jamie");
  });

  it("asks for the edit form with the flag the page opens on", () => {
    expect(agentHref("jamie", { edit: true })).toBe("#/team/jamie?edit");
  });

  it("escapes an id that would otherwise change the address", () => {
    // A tenant-namespaced id carries characters the hash reads structurally;
    // an unescaped `/` would name a teammate of some other view entirely.
    expect(agentHref("acme/ceo")).toBe("#/team/acme%2Fceo");
  });
});
