/** The media grant is optional in the host response, so absent must stay unknown. */
import { describe, expect, it } from "vitest";

import type { CapabilityStatusDto } from "@/api/types";
import { mediaStatus } from "@/views/UsageView";

function granted(over: Partial<CapabilityStatusDto> = {}): CapabilityStatusDto {
  return {
    configured: false,
    mediaInBuild: true,
    mediaGranted: true,
    mediaCredentialConfigured: true,
    ...over,
  };
}

describe("mediaStatus", () => {
  it("reports an omitted grant as unknown, never as not granted", () => {
    expect(mediaStatus(granted({ mediaGranted: undefined }))).toEqual({
      label: "Couldn't check",
      variant: "outline",
    });
  });

  it("reports an explicit false grant as not granted", () => {
    expect(mediaStatus(granted({ mediaGranted: false }))).toEqual({
      label: "Not granted",
      variant: "secondary",
    });
  });
});
