import { describe, expect, it } from "vitest";

import { readLedgerViewMode } from "@/hooks/use-ledger-view-mode";

describe("ledger view mode route state", () => {
  it("selects List only when the hash explicitly asks for it", () => {
    expect(readLedgerViewMode("#/ledgers/tasks?view=list")).toBe("list");
    expect(readLedgerViewMode("#/ledgers/tasks?view=board")).toBe("board");
    expect(readLedgerViewMode("#/ledgers/tasks?new")).toBe("board");
  });

  it("defaults to the ledger's own mode when the hash names no view", () => {
    // A declared ledger opens as rows; the tasks ledger opens as a board —
    // that is `defaultLedgerMode`, passed in as the fallback (issue #1351).
    expect(readLedgerViewMode("#/ledgers/goals", "list")).toBe("list");
    // An explicit `?view=list` keeps winning over the fallback.
    expect(readLedgerViewMode("#/ledgers/tasks?view=list", "board")).toBe("list");
  });

  it("recognizes an explicit board override on a row-default ledger", () => {
    // A declared ledger's fallback is rows, so the address must be able to say
    // `view=board`. Treating an unparsed value as the fallback would make the
    // board snap back to rows on the next `hashchange` and become unreachable
    // after one Board → List → Board toggle (issue #1397).
    expect(readLedgerViewMode("#/ledgers/goals?view=board", "list")).toBe("board");
    expect(readLedgerViewMode("#/ledgers/goals?view=list", "list")).toBe("list");
  });
});
