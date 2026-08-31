/**
 * Issue #886 — the Composio row on the Usage view must never paint a working
 * connector red.
 *
 * The credential resolves over three tiers (a BYO Composio token, the company's
 * TinyHumans key, this instance's platform identity). The row used to read
 * `composioTokenConfigured`, which answers only the first, so a hosted tenant
 * running on the platform identity got "Awaiting token" in the alarm colour
 * while its agents were calling `GITHUB_*` tools successfully.
 *
 * The state that matters most here is the one with no obvious label: the host
 * not answering. It is a separate rung above `"none"` precisely so it cannot
 * fall through into the destructive branch and re-create the bug.
 */
import { describe, expect, it } from "vitest";

import type { CapabilityStatusDto } from "@/api/types";
import { composioStatus } from "@/views/UsageView";

/** A granted, in-build company — the only shape the credential rungs are reached from. */
function granted(over: Partial<CapabilityStatusDto> = {}): CapabilityStatusDto {
  return {
    configured: false,
    composioInBuild: true,
    composioGranted: true,
    ...over,
  };
}

describe("composioStatus", () => {
  it("reports a build without the feature before anything else", () => {
    expect(
      composioStatus(granted({ composioInBuild: false, composioCredentialSource: "attested" })),
    ).toEqual({ label: "Not in this build", variant: "outline" });
  });

  it("reports an ungranted company without consulting the credential", () => {
    expect(
      composioStatus(granted({ composioGranted: false, composioCredentialSource: "attested" })),
    ).toEqual({ label: "Not granted", variant: "secondary" });
  });

  it("reports an omitted grant as unknown, never as not granted", () => {
    expect(composioStatus(granted({ composioGranted: undefined }))).toEqual({
      label: "Couldn't check",
      variant: "outline",
    });
  });

  /**
   * The #886 regression guard. An unanswered host is unknown, not broken —
   * and specifically not `destructive`, which is the colour that sent the
   * original debugging in the wrong direction.
   */
  it("reports an unanswered host as unknown, never as an alarm", () => {
    const status = composioStatus(granted({ composioCredentialSource: undefined }));
    expect(status.label).toBe("Couldn't check");
    expect(status.variant).not.toBe("destructive");
  });

  it("reports a genuinely unresolvable credential as the destructive state", () => {
    expect(composioStatus(granted({ composioCredentialSource: "none" }))).toEqual({
      label: "Awaiting credential",
      variant: "destructive",
    });
  });

  /**
   * All three resolving tiers are Active. `attested` is the hosted shape the
   * issue was reported against, and it is the one the old code got wrong:
   * nothing is stored on the instance, so `composioTokenConfigured` is `false`
   * while the toolbelt is fully wired.
   */
  it.each(["attested", "company", "static"] as const)(
    "reports a resolved `%s` credential as active",
    (source) => {
      expect(
        composioStatus(granted({ composioCredentialSource: source, composioTokenConfigured: false })),
      ).toEqual({ label: "Active", variant: "default" });
    },
  );

  /**
   * The narrow legacy field must not be able to steer the verdict in either
   * direction: it answers "did somebody paste a BYO token", which is a
   * different question from "does a credential resolve".
   */
  it("ignores the BYO-token flag once the resolver has answered", () => {
    expect(
      composioStatus(granted({ composioTokenConfigured: true, composioCredentialSource: "none" }))
        .variant,
    ).toBe("destructive");
    expect(
      composioStatus(granted({ composioTokenConfigured: false, composioCredentialSource: "attested" }))
        .label,
    ).toBe("Active");
  });
});
