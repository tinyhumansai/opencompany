import { describe, expect, it } from "vitest";

import { formatUsdCost } from "@/lib/cost";

describe("formatUsdCost", () => {
  it("keeps sub-cent timeline spend visible", () => {
    expect(formatUsdCost({ amountUsd: 0.0106 }, "line")).toBe("$0.0106");
    expect(formatUsdCost({ amountUsd: 0.00001 }, "line")).toBe("<$0.0001");
  });

  it("uses compact totals without rendering positive spend as zero", () => {
    expect(formatUsdCost({ amountUsd: 0.42 }, "total")).toBe("$0.42");
    expect(formatUsdCost({ amountUsd: 0.001 }, "total")).toBe("<$0.01");
  });

  it("distinguishes redaction from zero usage", () => {
    expect(formatUsdCost({ hidden: true }, "total")).toBe("Cost hidden");
    expect(formatUsdCost({ amountUsd: 0 }, "total")).toBeNull();
  });
});
