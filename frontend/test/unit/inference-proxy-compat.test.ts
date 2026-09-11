// The proxy-compatibility rule, as a rule rather than as a rendered form.
//
// It used to be reachable only through the single-provider form, where thirty
// tests drove a select, a text input and a Save button to reach nine assertions
// about one predicate. The form is retired; the rule moved to
// `@/inference/proxy-compat` unchanged, and this is what those tests were
// protecting, said directly.
//
// **Each case names the instance it comes from.** The rule is nine recorded
// regressions deep (issue #1838 and its follow-ups) and every one of them is a
// value that was either wrongly kept or wrongly dropped. A case deleted here is
// a regression re-opened.

import { describe, expect, it } from "vitest";

import {
  PROXIED_SLUG,
  isProxyCompatible,
  overrideIsSendable,
  stripProxyIncompatible,
} from "@/inference/proxy-compat";

describe("the two shapes the subscription proxy accepts", () => {
  it("accepts a bare tier name, which is not really an override at all", () => {
    // `model_for_tier` reads a bare tier as "let the platform resolve this
    // tier itself".
    for (const tier of ["chat-v1", "reasoning-v1", "agentic-v1", "vision-v1"]) {
      expect(isProxyCompatible(tier)).toBe(true);
    }
  });

  it("accepts the proxy's own three-segment passthrough form", () => {
    expect(isProxyCompatible("openrouter/anthropic/claude-sonnet-5")).toBe(true);
  });

  it("rejects a raw two-segment registry id (ninth instance)", () => {
    // `model_for_tier` forwards an override verbatim on the proxied path
    // whatever its shape, and the platform's curated tier registry does not
    // know it.
    expect(isProxyCompatible("anthropic/claude-sonnet-5")).toBe(false);
  });

  it("rejects a bare unnamespaced id typed out of direct-path habit (ninth instance)", () => {
    // The bug the whitelist replaced a blacklist to close: an earlier version
    // asked "is this shaped like a raw catalog id?" and treated every slashless
    // string as a safe bare-tier passthrough. `gpt-4o` rode straight through
    // Save and the request failed instead of the value being dropped the way
    // the warning promised.
    for (const typed of ["gpt-4o", "llama3", "claude"]) {
      expect(isProxyCompatible(typed), typed).toBe(false);
    }
  });

  it("counts the segments rather than trusting the openrouter/ prefix (fifth instance)", () => {
    // OpenRouter's own registry owns two-segment ids under that author name.
    // A plain `startsWith` mistook `openrouter/auto` for the proxy's
    // three-segment form and let a raw registry id through under the exemption
    // meant for ids the proxy actually accepts.
    expect(isProxyCompatible("openrouter/auto")).toBe(false);
    expect(isProxyCompatible("openrouter/anthropic/claude-sonnet-5")).toBe(true);
  });

  it("trims before any shape check (seventh instance)", () => {
    // A pasted value keeps its whitespace until a later pass. Untrimmed, this
    // failed `startsWith("openrouter/")` on the leading space and was silently
    // dropped even though it is exactly the form the proxy accepts.
    expect(isProxyCompatible(" openrouter/anthropic/claude-sonnet-5 ")).toBe(true);
    expect(isProxyCompatible("  chat-v1  ")).toBe(true);
  });

  it("is shape-based, not catalog-membership-based (third and fourth instances)", () => {
    // Membership only answers the question once a catalog has loaded, and a raw
    // id breaks the proxy whether or not it happens to appear in whatever
    // snapshot was fetched. Keying off the shape gives every caller the same
    // answer with no dependency on a network read having resolved — which is
    // why this test needs no catalog at all.
    expect(isProxyCompatible("anthropic/claude-sonnet-5")).toBe(false);
  });
});

describe("stripping a whole tier map", () => {
  it("is the one place a kept value is trimmed (seventh instance)", () => {
    // Callers used to trim afterwards, and one of them — Remove Key — sent its
    // carried models straight to the wire with no trim pass of its own, so a
    // pasted id survived with its whitespace intact.
    expect(
      stripProxyIncompatible({ "chat-v1": " openrouter/anthropic/claude-sonnet-5 " }),
    ).toEqual({ "chat-v1": "openrouter/anthropic/claude-sonnet-5" });
  });

  it("drops the incompatible entries and keeps the rest", () => {
    expect(
      stripProxyIncompatible({
        "chat-v1": "anthropic/claude-sonnet-5",
        "reasoning-v1": "openrouter/anthropic/claude-opus-5",
        "agentic-v1": "agentic-v1",
        "vision-v1": "",
      }),
    ).toEqual({
      "reasoning-v1": "openrouter/anthropic/claude-opus-5",
      "agentic-v1": "agentic-v1",
    });
  });

  it("is one implementation, so the rule cannot drift between call sites", () => {
    // Three separate partial implementations of this rule had already gone out
    // of step with each other before it was funnelled through one function.
    const map = { "chat-v1": "openrouter/auto" };
    expect(stripProxyIncompatible(map)).toEqual({});
    expect(isProxyCompatible("openrouter/auto")).toBe(false);
  });
});

describe("which provider the rule applies to", () => {
  it("applies to the platform proxy and to nothing else", () => {
    // A tenant's own OpenRouter account, a custom endpoint or a local runtime
    // takes whatever id the operator types, verbatim. There is nothing to check
    // it against — a vendor's catalog is the vendor's business.
    expect(overrideIsSendable(PROXIED_SLUG, "anthropic/claude-sonnet-5")).toBe(false);
    expect(overrideIsSendable("openrouter", "anthropic/claude-sonnet-5")).toBe(true);
    expect(overrideIsSendable("acme", "gpt-4o")).toBe(true);
  });

  it("treats an empty override as sendable everywhere", () => {
    // Empty means "no override", which every endpoint accepts.
    expect(overrideIsSendable(PROXIED_SLUG, "")).toBe(true);
    expect(overrideIsSendable(PROXIED_SLUG, "   ")).toBe(true);
  });

  it("judges a settled value, which is why a half-typed id survives (sixth instance)", () => {
    // Six of the nine instances are about stripping mid-keystroke. The
    // predicate is deliberately asked on save rather than on change, and the
    // prefix of a good value is only judged when the operator stops typing.
    expect(overrideIsSendable(PROXIED_SLUG, "openrouter/anthropic/claude-sonnet-5")).toBe(true);
  });
});
