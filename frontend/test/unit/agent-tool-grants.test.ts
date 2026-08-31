import { describe, expect, it } from "vitest";

import { companyCovers, grantCeiling, isEditable, parseToolGlobs, toolGlobsDiffer } from "@/lib/agent";
import type { AgentDetailDto } from "@/api/types";

/**
 * The Tools card became an editor, and everything it decides before sending a
 * `PATCH` is decided here.
 *
 * The card used to be a read-only report: it showed what a teammate asked for,
 * what the company allowed, and — struck through — what had been dropped
 * between the two, with no way to act on any of it. The host has accepted a
 * `tools` key on the agent patch for admins the whole time. These are the
 * derivations that stand between an operator's typing and that write, and each
 * fails silently rather than loudly when it is wrong: a mis-split glob is
 * stored happily and confers nothing, and a coverage hint that lies sends
 * somebody to edit the wrong list.
 */

function agent(over: Partial<AgentDetailDto> = {}): AgentDetailDto {
  return {
    id: "jamie",
    name: "Jamie",
    role: "Growth",
    description: "Runs paid acquisition.",
    source: "overlay",
    editable: ["name", "role", "description", "instructions", "tools"],
    isOrchestrator: false,
    tools: { requested: [], companyAllow: ["*"], deskAllow: [], deskCeilingActive: false, effective: ["*"] },
    desks: [],
    inboxEnabled: false,
    ...over,
  };
}

describe("parseToolGlobs", () => {
  it("splits on commas and whitespace alike", () => {
    // Both spellings are what people type after reading a `company.toml`.
    // Splitting on commas alone would store a grant literally named `docs.*,`,
    // which matches nothing and reports as "asked for but not granted".
    expect(parseToolGlobs("docs.*, files.*")).toEqual(["docs.*", "files.*"]);
    expect(parseToolGlobs("docs.*  files.*")).toEqual(["docs.*", "files.*"]);
    expect(parseToolGlobs("docs.*,files.*\nsearch")).toEqual(["docs.*", "files.*", "search"]);
  });

  it("collapses duplicates and keeps first-seen order", () => {
    expect(parseToolGlobs("search, docs.*, search")).toEqual(["search", "docs.*"]);
  });

  it("reads a blank field as an empty list, which since #1804 is a deny-all", () => {
    // The parse is unchanged — a blank field is `[]` — but its meaning inverted:
    // since #1804 `[]` is a deliberate deny-all ("no tools"), NOT the standard
    // grant. The standard grant is `null`, reached via "Reset to standard grant".
    // This must still not be confused with a parse failure.
    expect(parseToolGlobs("   ")).toEqual([]);
    expect(parseToolGlobs(",, ,")).toEqual([]);
  });
});

describe("toolGlobsDiffer", () => {
  it("ignores re-ordering and re-spacing", () => {
    expect(toolGlobsDiffer(["a", "b"], parseToolGlobs("b, a"))).toBe(false);
  });

  it("notices an addition, a removal, and a clear", () => {
    expect(toolGlobsDiffer(["a"], ["a", "b"])).toBe(true);
    expect(toolGlobsDiffer(["a", "b"], ["a"])).toBe(true);
    expect(toolGlobsDiffer(["a"], [])).toBe(true);
  });

  it("treats a duplicate stored entry as the same grant as its single entry", () => {
    // The stored list is never re-parsed, so a manifest may hold `["search",
    // "search"]` while the editor's parsed view collapses it to `["search"]`.
    // Comparing raw lengths would make the card claim a change and rewrite the
    // stored list even though the grant set is identical.
    expect(toolGlobsDiffer(["search", "search"], ["search"])).toBe(false);
    expect(toolGlobsDiffer(["search", "docs.*", "search"], ["search", "docs.*"])).toBe(false);
  });
});

describe("companyCovers", () => {
  it("treats the catch-all as covering the ordinary families", () => {
    expect(companyCovers(["*"], "docs.*")).toBe(true);
    expect(companyCovers(["*"], "workspace.read")).toBe(true);
    expect(companyCovers(["*"], "workspace.write")).toBe(false);
  });

  it("does not let the catch-all cover the explicit opt-in namespaces", () => {
    // The host's `allow_covers` rejects these under a bare `*`, and the hint
    // must agree or it would promise a grant that never lands. A dotted
    // descendant ask is as much an opt-in as the bare namespace — it must not
    // fall through to the generic matcher, where the wildcard would cover it.
    expect(companyCovers(["*"], "search")).toBe(false);
    expect(companyCovers(["*"], "search.web")).toBe(false);
    expect(companyCovers(["*"], "media")).toBe(false);
    expect(companyCovers(["*"], "media.image")).toBe(false);
    expect(companyCovers(["*"], "composio")).toBe(false);
    expect(companyCovers(["*"], "composio.gmail")).toBe(false);
    expect(companyCovers(["*"], "chargebee")).toBe(false);
    expect(companyCovers(["*"], "chargebee.read")).toBe(false);
    expect(companyCovers(["*"], "hosting")).toBe(false);
    expect(companyCovers(["*"], "hosting.deploy")).toBe(false);
    expect(companyCovers(["*"], "paypal")).toBe(false);
    expect(companyCovers(["*"], "paypal.wallet")).toBe(false);
    expect(companyCovers(["*"], "mcp:*")).toBe(false);
    expect(companyCovers(["*"], "mcp:notion")).toBe(false);
    expect(companyCovers(["*"], "mcp*")).toBe(false);
  });

  it("treats the bare workspace grant as explicit-only, not a catch-all read", () => {
    // `grants_workspace_write_explicit` accepts the bare `workspace` token as a
    // write grant, so a request for it under a `["*"]` allow-list would hand the
    // agent the exact token the wiring predicate accepts. The hint must refuse
    // it the way it refuses `workspace.write` — reads still come from the
    // catch-all, writes never do. (A `workspace.*` *request* is a different
    // token: it strips to `workspace.` and survives as an effective `workspace.*`
    // that the exact-token write predicate rejects, so the catch-all covering it
    // confers nothing beyond reads.)
    expect(companyCovers(["*"], "workspace")).toBe(false);
    expect(companyCovers(["*"], "workspace.*")).toBe(true);
    expect(companyCovers(["*"], "workspace.read")).toBe(true);
    // An explicit `workspace` or `workspace.write` allow covers the write
    // request in either spelling.
    expect(companyCovers(["workspace"], "workspace")).toBe(true);
    expect(companyCovers(["workspace"], "workspace.write")).toBe(true);
    expect(companyCovers(["workspace.write"], "workspace")).toBe(true);
    expect(companyCovers(["workspace.write"], "workspace.write")).toBe(true);
    // A read-only grant does not cover the write-inclusive bare request.
    expect(companyCovers(["workspace.read"], "workspace")).toBe(false);
    // And a bare `workspace` allow does not cover a `workspace.*` request: the
    // request strips to `workspace.` and falls to the generic matcher, where an
    // unstarred grant matches only itself. The host's `allow_covers` resolves
    // the same way, so the hint's "will not apply" warning for this pair is the
    // truth, not a false negative — a company that allows only writes still has
    // to add `workspace.*` before a teammate's read-glob ask lands.
    expect(companyCovers(["workspace"], "workspace.*")).toBe(false);
  });

  it("covers an explicit opt-in only from a grant that names it", () => {
    expect(companyCovers(["search"], "search")).toBe(true);
    expect(companyCovers(["search.*"], "search.web")).toBe(true);
    expect(companyCovers(["media"], "media")).toBe(true);
    expect(companyCovers(["media.*"], "media")).toBe(true);
    expect(companyCovers(["composio"], "composio")).toBe(true);
    expect(companyCovers(["chargebee"], "chargebee.read")).toBe(true);
    expect(companyCovers(["hosting"], "hosting.deploy")).toBe(true);
    expect(companyCovers(["paypal.wallet"], "paypal")).toBe(true);
    expect(companyCovers(["mcp:*"], "mcp:notion")).toBe(true);
    expect(companyCovers(["mcp:notion"], "mcp:notion")).toBe(true);
    // …but a *different* namespace does not confer it.
    expect(companyCovers(["media.generation"], "composio")).toBe(false);
    // …while the opt-in predicate accepts any sub-grant of the namespace, so
    // `search.web` does confer a bare `search` request — unlike the generic
    // matcher, where `docs.read` would not confer `docs`.
    expect(companyCovers(["search.web"], "search")).toBe(true);
    // …and the bare namespace grant covers its dotted descendants, matching
    // `grants_search_explicit` (which `search` and `search.web` both satisfy).
    expect(companyCovers(["search"], "search.web")).toBe(true);
    expect(companyCovers(["media"], "media.image")).toBe(true);
    expect(companyCovers(["composio"], "composio.gmail")).toBe(true);
    expect(companyCovers(["chargebee"], "chargebee.read")).toBe(true);
    expect(companyCovers(["hosting.*"], "hosting.deploy")).toBe(true);
    expect(companyCovers(["paypal"], "paypal.wallet")).toBe(true);
  });

  it("covers a sub-grant from a starred namespace", () => {
    expect(companyCovers(["workspace.*"], "workspace.read")).toBe(true);
  });

  it("does not cover a bare namespace from its starred form", () => {
    // The asymmetry that makes manifests list `"workspace", "workspace.*"` as
    // two entries. It reads like a bug and is the host's actual rule, so the
    // hint has to have it too or it would promise a grant that never lands.
    expect(companyCovers(["workspace.*"], "workspace")).toBe(false);
  });

  it("does not cover a star glued to an opt-in namespace", () => {
    // `search*` and `workspace.write*` are stored verbatim by the write path,
    // and the wiring predicates reject the glued spelling — the card would
    // render the saved grant as effective while the tools stay unwired. The
    // preview must not promise them, even when the company holds the namespace.
    const allow = ["search", "workspace", "media", "composio", "chargebee", "hosting", "paypal", "mcp:*"];
    for (const glob of ["search*", "workspace*", "workspace.write*", "media*", "composio*", "chargebee*", "hosting*", "paypal*", "mcp*"]) {
      expect(companyCovers(allow, glob)).toBe(false);
    }
  });

  it("keeps the separator-broken opt-in spellings covered", () => {
    // `search.web*` strips to a `search.`-descendant the predicate accepts
    // verbatim; `workspace.write` is an exact write token; `mcp:notion*` is a
    // colon-scoped prefix. These all wire when saved, so they stay covered.
    const allow = ["search", "workspace", "media", "mcp:*"];
    expect(companyCovers(allow, "search.*")).toBe(true);
    expect(companyCovers(allow, "search.web*")).toBe(true);
    expect(companyCovers(allow, "workspace.write")).toBe(true);
    expect(companyCovers(allow, "media.*")).toBe(true);
    expect(companyCovers(allow, "media.image*")).toBe(true);
    expect(companyCovers(allow, "mcp:notion*")).toBe(true);
  });

  it("stops a prefix that does not end on a separator", () => {
    // `documentation.read` is not a `docs` grant, however much of the string
    // lines up — and an unstarred grant matches only itself.
    expect(companyCovers(["docs*"], "documentation.read")).toBe(false);
    // And an unstarred grant matches only itself, sub-grants included — the
    // same rule that makes a manifest list `"workspace", "workspace.*"`. The
    // opt-in namespaces are the exception, exercised in the test above.
    expect(companyCovers(["docs"], "docs.read")).toBe(false);
  });

  it("reports an uncovered ask, which is the whole warning", () => {
    // `*` covers the ordinary families but not the opt-ins, so a company
    // allowing it still has to name `search` before a teammate can hold it.
    expect(companyCovers(["*", "media"], "search")).toBe(false);
    expect(companyCovers(["docs.*", "files.*"], "search")).toBe(false);
    expect(companyCovers([], "docs.*")).toBe(false);
  });
});

describe("grantCeiling", () => {
  it("is the company allow-list when no desk states a ceiling", () => {
    // `deskCeilingActive` false means no desk narrows anything — the same
    // empty-is-not-nothing trap `requested` carries, not "this desk grants no
    // tools". The company list is the whole gate.
    expect(
      grantCeiling({
        requested: [],
        companyAllow: ["*", "media"],
        deskAllow: [],
        deskCeilingActive: false,
        effective: ["*", "media"],
      }),
    ).toEqual(["*", "media"]);
  });

  it("is the desk allowance when a desk states a ceiling", () => {
    // The marketing agency's creative desk omits `media` while the company
    // allows it, so the desk allowance (already company-narrowed on the host)
    // is the gate an editor draft has to clear.
    expect(
      grantCeiling({
        requested: [],
        companyAllow: ["*", "media"],
        deskAllow: ["*"],
        deskCeilingActive: true,
        effective: ["*"],
      }),
    ).toEqual(["*"]);
  });

  it("keeps the desk level as the gate when an active ceiling resolves to empty", () => {
    // A company allowing only `*` with a desk naming only `media` (an explicit
    // opt-in a bare `*` does not confer) narrows everything away. `deskAllow`
    // is empty but the ceiling is still active, so the preview must not fall
    // back to `companyAllow` and promise `docs.*` will apply — the host drops
    // it and the saved grant confers nothing.
    expect(
      grantCeiling({
        requested: [],
        companyAllow: ["*"],
        deskAllow: [],
        deskCeilingActive: true,
        effective: [],
      }),
    ).toEqual([]);
  });

  it("lets the desk ceiling narrow what an editor warns about", () => {
    // The whole point of the preview: a grant the company allows but the desk
    // omits is stored happily and then dropped immediately after saving, so the
    // live hint must flag it while typing. `media` is company-allowed but not
    // on the creative desk, so `companyCovers` against the desk allowance says
    // it will not apply — exactly what `willNotApply` renders.
    const ceiling = grantCeiling({
      requested: [],
      companyAllow: ["*", "media"],
      deskAllow: ["*"],
      deskCeilingActive: true,
      effective: ["*"],
    });
    expect(companyCovers(ceiling, "media")).toBe(false);
    expect(companyCovers(ceiling, "docs.*")).toBe(true);
  });
});

describe("isEditable", () => {
  it("accepts `tools`, which the Tools card's editor sends on save", () => {
    expect(isEditable(agent(), "tools")).toBe(true);
    // A member gets every other key and not this one — the host gates `tools`
    // on admin because an empty list is a potential widening.
    expect(isEditable(agent({ editable: ["name", "role", "description"] }), "tools")).toBe(false);
  });
});
