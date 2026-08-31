import { describe, expect, it } from "vitest";

import {
  approvalAction,
  decisionLabel,
  grantHeadline,
  payloadLeadTruncated,
  payloadLines,
  toolAction,
} from "@/lib/language";
import type { ApprovalSummary, StandingGrant } from "@/api/types";

/**
 * The gated tools an operator is asked to sign off on say what they are (#701).
 *
 * The Rust guard (`every_consequence_tool_has_a_console_label` in
 * `src/policy/consequence.rs`) asserts that each of these tools has a *key* in
 * one of the label tables — that is the half which cannot be checked from here,
 * because the declaration table it reads is Rust. This is the other half: that
 * the key is reachable through the resolution the console actually performs.
 *
 * Both are needed. A key in `TOOL_LABELS` satisfies the guard while remaining
 * invisible if `approvalAction`'s rung order ever changed, and the whole defect
 * class (#372, #551 → #671, #701) is labels that exist somewhere and reach
 * nobody.
 */

function approval(
  over: Partial<ApprovalSummary> & Pick<ApprovalSummary, "kind">,
): ApprovalSummary {
  return {
    id: "a1",
    amount_usd: null,
    at_millis: 1_000,
    agent: "ceo",
    ...over,
  };
}

/** What each per-call tool gate asks for, keyed by the kind it parks under. */
const PER_CALL: Record<string, string> = {
  curl: "Download a file from the internet",
  http_request: "Make a request to a web address",
  git_operations: "Run a git command in its workspace",
  read_workspace_state: "Check its workspace's git status",
  mcp_call_tool: "Use a tool on a connected server",
  publish_artifact: "Publish a file it produced",
  run_workflow: "Run one of its saved workflows",
};

/** The four an operator may grant standing on, so the #374 list renders them. */
const GRANTABLE: Record<string, string> = {
  file_write: "Write a file in its workspace",
  edit: "Edit a file in its workspace",
  apply_patch: "Edit several files in its workspace at once",
  csv_export: "Save data as a spreadsheet file in its workspace",
};

describe("an approval card for a gated tool", () => {
  for (const [kind, sentence] of Object.entries(PER_CALL)) {
    it(`says what \`${kind}\` is asking for`, () => {
      expect(approvalAction(approval({ kind }))).toBe(sentence);
    });
  }

  it("never leaves one of them on the generic fallback", () => {
    for (const kind of [...Object.keys(PER_CALL), ...Object.keys(GRANTABLE)]) {
      expect(approvalAction(approval({ kind }))).not.toBe(
        "Use one of its tools",
      );
    }
  });

  it("still resolves a business effect through the effect glossary first", () => {
    // Rung 1 is unchanged by #701 — nothing was added to EFFECT_LABELS, and a
    // tool label that shadowed one of its entries would be a silent rewording of
    // a card that has read the same way since #372.
    expect(approvalAction(approval({ kind: "payment.send" }))).toBe(
      "Send a payment",
    );
    expect(approvalAction(approval({ kind: "mcp_registry_tool_call" }))).toBe(
      "Use a connected tool",
    );
  });
});

describe("the standing permissions list", () => {
  for (const [kind, sentence] of Object.entries({
    ...PER_CALL,
    ...GRANTABLE,
  })) {
    it(`names \`${kind}\` without an approval to read it from`, () => {
      expect(toolAction(kind)).toBe(sentence);
    });
  }

  it("tells the four grantable tools apart", () => {
    // The point of labelling them: this list has no payload block, so two rows
    // reading the same sentence are two permissions an operator cannot choose
    // between.
    const rendered = Object.keys(GRANTABLE).map(toolAction);
    expect(new Set(rendered).size).toBe(rendered.length);
  });
});

describe("the payload block underneath the label", () => {
  // The labels for these three are the shortest of the eleven precisely because
  // the address is right below them. If the ordering regresses, the card still
  // renders — it just buries the one argument being consented to under a header
  // map, which is the failure no type catches.
  it("leads a request with its address, not with its headers", () => {
    const lines = payloadLines(
      approval({
        kind: "http_request",
        payload: {
          headers: { Authorization: "…" },
          body: "{}",
          url: "https://x.test",
          method: "POST",
        },
      }),
    );
    expect(lines.map((l) => l.label)).toStrictEqual([
      "url",
      "method",
      "headers",
      "body",
    ]);
  });

  it("leads a download with its address", () => {
    const lines = payloadLines(
      approval({
        kind: "curl",
        payload: { dest_path: "out.bin", url: "https://x.test/f" },
      }),
    );
    expect(lines.map((l) => l.label)).toStrictEqual(["url", "dest_path"]);
  });

  it("leads a git call with the operation it is about to run", () => {
    const lines = payloadLines(
      approval({
        kind: "git_operations",
        payload: { message: "wip", operation: "commit" },
      }),
    );
    expect(lines[0]).toStrictEqual({ label: "operation", value: "commit" });
  });
});

describe("the decide buttons' label (#1411)", () => {
  const askers = new Map([["ceo", "Sam"]]);
  // Sixty seconds after the default `at_millis: 1_000` — a bucket the hidden
  // card tests can also read off (`composed 1m ago`).
  const NOW = 61_000;

  it("names the request behind the action, not just its kind", () => {
    expect(
      decisionLabel(
        approval({ kind: "shell", payload: { command: "make release" } }),
        askers,
        NOW,
      ),
    ).toBe("Run a terminal command — make release — asked by Sam");
  });

  it("tells two same-kind cards apart when their payloads differ", () => {
    // The whole point of the label: two shell commands with the same action
    // phrase must still offer distinguishable decide buttons.
    const first = decisionLabel(
      approval({ kind: "shell", payload: { command: "make release" } }),
      askers,
      NOW,
    );
    const second = decisionLabel(
      approval({ kind: "shell", payload: { command: "npm run deploy" } }),
      askers,
      NOW,
    );
    expect(first).not.toBe(second);
  });

  it("omits the asker when the card has no agent", () => {
    expect(
      decisionLabel(
        approval({
          kind: "shell",
          payload: { command: "make release" },
          agent: null,
        }),
        askers,
        NOW,
      ),
    ).toBe("Run a terminal command — make release");
  });

  it("omits the payload lead when the card has none", () => {
    expect(decisionLabel(approval({ kind: "payment.send" }), askers, NOW)).toBe(
      "Send a payment — asked by Sam",
    );
  });

  it("falls back to the asker id when the roster does not know it", () => {
    expect(
      decisionLabel(
        approval({ kind: "shell", payload: { command: "make" } }),
        new Map(),
        NOW,
      ),
    ).toBe("Run a terminal command — make — asked by ceo");
  });

  it("tells same-first-lines cards apart by the dropped argument, not the id", () => {
    // Two `http_request`s sharing url, method and headers differ only in the
    // body, which is what the line cap omits. The button must name that body's
    // start rather than the opaque card id — the same words the card body uses.
    const first = decisionLabel(
      approval({
        kind: "http_request",
        payload: {
          url: "https://x.test",
          method: "POST",
          headers: "a: b",
          body: '{"q": 1}',
        },
      }),
      askers,
      NOW,
    );
    const second = decisionLabel(
      approval({
        kind: "http_request",
        payload: {
          url: "https://x.test",
          method: "POST",
          headers: "a: b",
          body: '{"q": 2}',
        },
      }),
      askers,
      NOW,
    );
    expect(first).not.toBe(second);
    expect(first).toContain('body: {"q": 1}');
    expect(second).toContain('body: {"q": 2}');
    expect(first).not.toContain("card a1");
  });

  it("keeps unmapped-tool cards apart when their first argument name differs", () => {
    // Two same-kind approvals whose first argument NAME differs but whose
    // value is the same must not read alike: `{path: "/tmp/a"}` and
    // `{destination: "/tmp/a"}` are different requests, and a bounded label
    // that dropped the name would hand both cards the same button text (#1411).
    const withPath = decisionLabel(
      approval({
        kind: "some_tool_nobody_declared",
        payload: { path: "/tmp/a" },
      }),
      askers,
      NOW,
    );
    const withDestination = decisionLabel(
      approval({
        kind: "some_tool_nobody_declared",
        payload: { destination: "/tmp/a" },
      }),
      askers,
      NOW,
    );
    expect(withPath).not.toBe(withDestination);
    expect(withPath).toContain("path: /tmp/a");
    expect(withDestination).toContain("destination: /tmp/a");
  });

  it("says why there is no lead when the host withheld the contents", () => {
    // A non-admin's withheld card has no payload to lead with, but it must not
    // read as an ordinary no-argument approval — the resolve route accepts any
    // member, and the phrase is the one `ApprovalRow` already uses. The exact
    // timestamp appears once, inside the composition phrase; the caller's own
    // `request <timestamp>` suffix is what is omitted on redacted cards.
    expect(
      decisionLabel(
        approval({ kind: "payment.send", contents_hidden: true }),
        askers,
        NOW,
      ),
    ).toBe(
      "Send a payment — details hidden by your role — composed 1m ago (1000) — asked by Sam",
    );
  });

  it("keeps same-bucket hidden cards apart by the exact timestamp", () => {
    // The relative phrase is bucketed, so two hidden cards composed in the same
    // bucket read the same "composed 1m ago" — the exact timestamp in
    // parentheses is the discriminator that still tells their buttons apart.
    const first = decisionLabel(
      approval({ kind: "payment.send", contents_hidden: true }),
      askers,
      61_000,
    );
    const second = decisionLabel(
      approval({
        kind: "payment.send",
        contents_hidden: true,
        at_millis: 2_000,
      }),
      askers,
      61_000,
    );
    expect(first).not.toBe(second);
    expect(first).toContain("(1000)");
    expect(second).toContain("(2000)");
    expect(first).not.toContain("request 1000");
  });

  it("keeps two hidden cards' decide buttons apart by their composition time", () => {
    // Withheld contents are redacted to the same phrase on every card, so two
    // hidden approvals of the same kind from the same asker would read alike —
    // the composition time (non-sensitive, already on the card body) is what
    // tells their buttons apart (#1411).
    const earlier = decisionLabel(
      approval({ kind: "payment.send", contents_hidden: true }),
      askers,
      61_000,
    );
    const later = decisionLabel(
      approval({ kind: "payment.send", contents_hidden: true }),
      askers,
      3_601_000,
    );
    expect(earlier).not.toBe(later);
    expect(earlier).toContain("composed 1m ago");
    expect(later).toContain("composed 1h ago");
  });

  it("bounds one long value instead of becoming a wall of text", () => {
    // The host bounds each value at 2,000 characters, so a long shell command
    // must not stretch the button name into one. The clip keeps the command's
    // start and end, with an ellipsis for the middle.
    const label = decisionLabel(
      approval({ kind: "shell", payload: { command: "x".repeat(2_000) } }),
      askers,
      NOW,
    );
    expect(label.length).toBeLessThanOrEqual(160);
    expect(label.startsWith("Run a terminal command — ")).toBe(true);
    expect(label.endsWith(" — asked by Sam")).toBe(true);
    const lead = label.slice(
      "Run a terminal command — ".length,
      -" — asked by Sam".length,
    );
    expect(lead.startsWith("x".repeat(59))).toBe(true);
    expect(lead.endsWith("…" + "x".repeat(59))).toBe(true);
  });

  it("tells cards apart by a dropped entry past the first", () => {
    // Five payload entries with the first four identical: the first dropped
    // line (index 3) is the same, so a label carrying only it would leave both
    // cards indistinguishable. The further dropped entry rides along too.
    const first = decisionLabel(
      approval({
        kind: "http_request",
        payload: {
          url: "https://x.test",
          method: "POST",
          headers: "a: b",
          body: '{"q": 1}',
          extra1: "alpha",
        },
      }),
      askers,
      NOW,
    );
    const second = decisionLabel(
      approval({
        kind: "http_request",
        payload: {
          url: "https://x.test",
          method: "POST",
          headers: "a: b",
          body: '{"q": 1}',
          extra1: "beta",
        },
      }),
      askers,
      NOW,
    );
    expect(first).not.toBe(second);
    expect(first).toContain("extra1: alpha");
    expect(second).toContain("extra1: beta");
  });
});

describe("whether a payload lead was cut to fit the compact row", () => {
  const request = (body: string) =>
    approval({
      kind: "http_request",
      payload: { url: "https://x.test", method: "POST", body },
    });

  it("is false when no promoted extra is previewed", () => {
    expect(payloadLeadTruncated(request("small"))).toBe(false);
  });

  it("is false at exactly the preview bound", () => {
    // `preview` cuts strictly past `EXTRA_PREVIEW_MAX`, so the boundary itself
    // is the full value — nothing was hidden, and nothing may be gated.
    expect(payloadLeadTruncated(request("x".repeat(60)))).toBe(false);
  });

  it("is true when a promoted extra is cut", () => {
    // A body longer than the bound is shown as a preview, and a preview is not
    // something an operator may one-click Approve.
    expect(payloadLeadTruncated(request("x".repeat(61)))).toBe(true);
  });

  it("is false for a kind that promotes nothing past the lead", () => {
    expect(
      payloadLeadTruncated(
        approval({ kind: "web_fetch", payload: { url: "x".repeat(500) } }),
      ),
    ).toBe(false);
  });

  it("is false when there is no payload to show", () => {
    expect(payloadLeadTruncated(approval({ kind: "http_request" }))).toBe(
      false,
    );
  });
});

describe("a kind nobody has named", () => {
  it("says a teammate wants a tool rather than inventing one", () => {
    expect(
      approvalAction(approval({ kind: "some_tool_nobody_declared" })),
    ).toBe("Use one of its tools");
  });

  it("says less again when there is no teammate to name", () => {
    expect(
      approvalAction(approval({ kind: "some.native.effect", agent: null })),
    ).toBe("Do something that needs your sign-off");
  });

  it("falls back the same way from the permissions list", () => {
    expect(toolAction("some_tool_nobody_declared")).toBe(
      "Use one of its tools",
    );
  });
});

/**
 * A standing permission's headline (#457), and the scope suffix it carries.
 *
 * `StandingGrant.scope` is one string holding two kinds of value — a Composio
 * toolkit slug, and (since #673/#739) a URL origin for `web_fetch`. Issue #785
 * was the second kind going through the first kind's speller, so a host-scoped
 * grant rendered `Https://docs.rs`.
 *
 * Both kinds are asserted here on purpose: the toolkit case alone is what CI and
 * review already had, and it stayed green through the whole of #785.
 */
function grant(
  over: Partial<StandingGrant> & Pick<StandingGrant, "tool">,
): StandingGrant {
  return {
    id: "g1",
    agent: "ceo",
    verdict: "approve",
    granted_by: { kind: "user", id: "u1" },
    at_millis: 1_000,
    expires_at_millis: 2_000,
    ...over,
  };
}

describe("what a standing permission covers", () => {
  it("keeps a host scope verbatim, scheme and all", () => {
    expect(
      grantHeadline(grant({ tool: "web_fetch", scope: "https://docs.rs" })),
    ).toBe("Fetch a web page — https://docs.rs only");
  });

  it("keeps the scheme of every origin shape the host can mint", () => {
    // `standing_scope_of` mints `scheme://host[:port]` — lower-case, port kept.
    for (const origin of [
      "https://www.bbc.com",
      "http://localhost:8080",
      "https://docs.rs:443",
      "https://sub.domain.example.co.uk",
    ]) {
      expect(grantHeadline(grant({ tool: "web_fetch", scope: origin }))).toBe(
        `Fetch a web page — ${origin} only`,
      );
    }
  });

  it("still spells a toolkit slug out for an operator", () => {
    expect(
      grantHeadline(
        grant({ tool: "composio_execute", scope: "microsoft_teams" }),
      ),
    ).toBe("Act in one of its connected accounts — Microsoft Teams only");
  });

  it("says only the action when the grant narrows to nothing", () => {
    expect(grantHeadline(grant({ tool: "file_write" }))).toBe(
      "Write a file in its workspace",
    );
  });

  it("prefaces a standing refusal with 'Don't allow'", () => {
    // Issue #1458: the grants list now carries denials too, and the headline
    // has to say the permission is a refusal, not an allowance.
    expect(
      grantHeadline(
        grant({ tool: "web_fetch", verdict: "deny", scope: "https://docs.rs" }),
      ),
    ).toBe("Don't allow Fetch a web page — https://docs.rs only");
    expect(grantHeadline(grant({ tool: "file_write", verdict: "deny" }))).toBe(
      "Don't allow Write a file in its workspace",
    );
  });
});
