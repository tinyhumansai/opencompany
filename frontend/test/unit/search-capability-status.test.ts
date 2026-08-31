/**
 * The web-search row on the Usage view, once a company can bring its own search
 * provider.
 *
 * The row used to read one flag — is a MANAGED credential resolvable on this
 * host — which is now only half the question. A company searching through its
 * own Brave or Exa account is working on a host with no platform credential at
 * all, and painting that red would send an admin looking for a key that is not
 * the missing piece. It also is not bounded by the platform's daily cap, since
 * that cap bounds the platform's bill.
 */
import { describe, expect, it } from "vitest";

import type { CapabilityStatusDto } from "@/api/types";
import { searchStatus } from "@/views/UsageView";

/** A granted, in-build company — the only shape the credential rungs are reached from. */
function granted(over: Partial<CapabilityStatusDto> = {}): CapabilityStatusDto {
  return {
    configured: false,
    searchInBuild: true,
    searchGranted: true,
    ...over,
  };
}

describe("searchStatus", () => {
  it("reports a build without the harness before anything else", () => {
    expect(searchStatus(granted({ searchInBuild: false, searchProvider: "exa" }))).toEqual({
      label: "Not in this build",
      variant: "outline",
    });
  });

  it("reports an ungranted company without consulting any credential", () => {
    expect(
      searchStatus(granted({ searchGranted: false, searchCredentialConfigured: true })),
    ).toEqual({ label: "Not granted", variant: "secondary" });
  });

  it("reports an omitted grant as unknown, never as not granted", () => {
    expect(searchStatus(granted({ searchGranted: undefined }))).toEqual({
      label: "Couldn't check",
      variant: "outline",
    });
  });

  /** The regression this file exists for. */
  it("reports a company on its own provider as working with no managed credential", () => {
    const status = searchStatus(
      granted({ searchProvider: "brave", searchCredentialConfigured: false }),
    );
    expect(status.label).toBe("Own provider");
    expect(status.variant).not.toBe("destructive");
  });

  /** And the platform's daily cap does not describe somebody else's bill. */
  it("does not report a company on its own provider as paused by the platform cap", () => {
    expect(searchStatus(granted({ searchProvider: "exa", searchDailyCallCap: 0 })).label).toBe(
      "Own provider",
    );
  });

  it("treats `managed` as no company provider at all", () => {
    expect(
      searchStatus(granted({ searchProvider: "managed", searchCredentialConfigured: false })),
    ).toEqual({ label: "Awaiting credential", variant: "destructive" });
  });

  it("keeps the managed rungs for a company that configured nothing", () => {
    expect(searchStatus(granted({ searchCredentialConfigured: false }))).toEqual({
      label: "Awaiting credential",
      variant: "destructive",
    });
    expect(
      searchStatus(granted({ searchCredentialConfigured: true, searchDailyCallCap: 0 })),
    ).toEqual({ label: "Paused (cap 0)", variant: "destructive" });
    expect(
      searchStatus(granted({ searchCredentialConfigured: true, searchDailyCallCap: 25 })),
    ).toEqual({ label: "Active", variant: "default" });
  });
});
