/**
 * The decisions behind Connections → Search, exercised as plain functions.
 *
 * Every branch worth a test lives in `@/search-providers/resolve` or
 * `@/search-providers/classify` rather than inside a component, which is the
 * whole reason those files exist: "should this row offer Remove key?" is a
 * question with a wrong answer, and a wrong answer inside a `<DropdownMenu>` is
 * only reachable through a rendered page.
 *
 * The two that matter most, because they are corrections rather than ports:
 *
 *   - **"Remove key" is never offered on a row with no key.** SearXNG is an
 *     address, not an account.
 *   - **Managed never claims "Always on".** It is always the *fallback*, which
 *     is a different claim from always *working*: a deployment with no platform
 *     credential falls back to a surface that answers nothing.
 */

import { describe, expect, it } from "vitest";

import {
  CATALOGUE,
  endpointHost,
  optionDetail,
} from "@/search-providers/catalogue";
import {
  confirmCopy,
  describeProbe,
  describeTest,
  destroysCredential,
  healthLabel,
  testOutcome,
} from "@/search-providers/classify";
import {
  addOptions,
  controlsFor,
  hasNoProviders,
  managedIsOn,
  managedSubline,
  rowSubline,
} from "@/search-providers/resolve";
import type { ProbeClass, SearchProvider } from "@/search-providers/types";

function account(overrides: Partial<SearchProvider> = {}): SearchProvider {
  return {
    slug: "brave",
    label: "Brave Search",
    category: "account",
    enabled: true,
    keyConfigured: true,
    takesKey: true,
    takesEndpoint: false,
    endpoint: null,
    complete: true,
    isDefault: false,
    ...overrides,
  };
}

function searxng(overrides: Partial<SearchProvider> = {}): SearchProvider {
  return {
    slug: "searxng",
    label: "SearXNG",
    category: "self-hosted",
    enabled: true,
    keyConfigured: false,
    takesKey: false,
    takesEndpoint: true,
    endpoint: "https://search.acme.internal",
    complete: true,
    isDefault: false,
    ...overrides,
  };
}

describe("row controls", () => {
  it("never offers Remove key on a row that has no key", () => {
    // SearXNG has an address and no account. Offering to remove its key would
    // be offering to remove nothing.
    expect(controlsFor(searxng())).not.toContain("remove-key");
    expect(controlsFor(searxng())).not.toContain("replace-key");
    expect(controlsFor(searxng())).toContain("edit-endpoint");
  });

  it("offers Remove key only once a key is actually stored", () => {
    expect(controlsFor(account({ keyConfigured: false }))).not.toContain(
      "remove-key",
    );
    expect(controlsFor(account({ keyConfigured: true }))).toContain(
      "remove-key",
    );
  });

  it("does not offer an address field on an account provider", () => {
    // Brave's base URL is a constant in the vendored tool and its constructor
    // takes no URL at all, so the field would be one the request ignores.
    expect(controlsFor(account())).not.toContain("edit-endpoint");
  });

  it("offers Set as default only where it would change something", () => {
    expect(controlsFor(account({ isDefault: true }))).not.toContain(
      "make-default",
    );
    expect(controlsFor(account({ enabled: false }))).not.toContain(
      "make-default",
    );
    expect(controlsFor(account())).toContain("make-default");
  });

  it("always offers the toggle, a check and removal", () => {
    for (const provider of [
      account(),
      searxng(),
      account({ enabled: false }),
    ]) {
      expect(controlsFor(provider)).toEqual(
        expect.arrayContaining(["toggle", "test", "remove"]),
      );
    }
  });
});

describe("row sub-lines", () => {
  it("gives each row exactly one fact, chosen by what the row is", () => {
    expect(rowSubline(account())).toBe("•••• configured");
    expect(rowSubline(account({ keyConfigured: false }))).toBe("no key");
    expect(rowSubline(searxng())).toBe("search.acme.internal");
    expect(rowSubline(searxng({ endpoint: null }))).toBe("no address");
  });

  it("falls back to the raw value when an address will not parse", () => {
    expect(endpointHost("not a url")).toBe("not a url");
    expect(endpointHost(null)).toBe("");
  });
});

describe("the managed row", () => {
  it("never claims Always on", () => {
    // It is the fallback whether or not it works, so the badge is keyed on
    // whether a platform credential actually resolves.
    expect(managedIsOn(true, true)).toBe(true);
    expect(managedIsOn(true, false)).toBe(false);
    expect(managedIsOn(false, true)).toBe(false);
  });

  it("says which of the three states it is in", () => {
    expect(managedSubline(false, false, 100)).toBe(
      "This build has no search tools",
    );
    expect(managedSubline(true, false, 100)).toBe(
      "No managed credential on this deployment",
    );
    expect(managedSubline(true, true, 250)).toBe(
      "Metered — up to 250 searches a day",
    );
  });

  it("is a row whatever it resolves to, so the notice is only about records", () => {
    // The row renders either way — that is the component's job and it takes no
    // condition — so the only question left is whether this company has
    // connected anything of its own. A deployment with no managed credential
    // must not change the answer: when it did, an e2e that pinned the branch
    // was really pinning the runner's fixture.
    expect(hasNoProviders([])).toBe(true);
    expect(hasNoProviders([account()])).toBe(false);
  });
});

describe("the add dialog", () => {
  it("offers only what is not yet connected", () => {
    const options = addOptions([account(), searxng()]);
    expect(options.account.map((option) => option.value)).toEqual([
      "exa",
      "querit",
    ]);
    expect(options.selfHosted).toEqual([]);
  });

  it("splits the catalogue into the two questions it asks", () => {
    const options = addOptions([]);
    expect(options.account.map((option) => option.value)).toEqual([
      "brave",
      "exa",
      "querit",
    ]);
    expect(options.selfHosted.map((option) => option.value)).toEqual([
      "searxng",
    ]);
  });

  it("shows an address for an account and a statement for a self-hosted one", () => {
    const brave = CATALOGUE.find((item) => item.slug === "brave");
    const searx = CATALOGUE.find((item) => item.slug === "searxng");
    expect(optionDetail(brave!)).toBe("api.search.brave.com");
    expect(optionDetail(searx!)).toBe("Runs on your own network");
  });

  it("ships no key-shape validation, because no provider documents one", () => {
    for (const item of CATALOGUE) {
      expect(item).not.toHaveProperty("keyPrefix");
      expect(item).not.toHaveProperty("keyPattern");
    }
  });
});

describe("probe copy", () => {
  it("destroys a credential for exactly one class", () => {
    expect(destroysCredential("auth")).toBe(true);
    for (const other of [
      "format",
      "quota",
      "endpoint",
      "timeout",
      "unknown",
    ] as ProbeClass[]) {
      expect(destroysCredential(other)).toBe(false);
    }
  });

  it("colours a kept credential as a warning, not an error", () => {
    // The save succeeded and only reachability is in question. Red would be a
    // lie about what happened.
    expect(describeProbe("auth", "Brave Search").tone).toBe("error");
    for (const other of [
      "format",
      "quota",
      "endpoint",
      "timeout",
      "unknown",
    ] as ProbeClass[]) {
      const advisory = describeProbe(other, "Brave Search");
      expect(advisory.tone).toBe("warning");
      expect(advisory.keyKept).toBe(true);
      expect(advisory.rowCreated).toBe(true);
    }
  });

  it("tells a SearXNG operator what to actually change", () => {
    // A 403 from SearXNG means JSON output is off, not that a key was rejected —
    // there is no key. Saying "unchecked" would throw away the only message that
    // could fix it.
    expect(describeProbe("format", "SearXNG").message).toContain(
      "search.formats",
    );
    expect(healthLabel("format")).toBe("JSON output off");
  });

  it("keeps test copy distinct from save copy", () => {
    // A test changes nothing, so "Saved, but…" would be wrong — while the
    // distinction between classes is still the entire point.
    for (const probeClass of [
      "auth",
      "format",
      "quota",
      "endpoint",
      "timeout",
      "unknown",
    ] as ProbeClass[]) {
      expect(describeTest(probeClass, "Exa")).not.toContain("Saved");
    }
    expect(describeTest("auth", "Exa")).not.toBe(
      describeTest("endpoint", "Exa"),
    );
  });

  it("never carries an upstream string into a sentence", () => {
    // These land in a banner somebody screenshots into a ticket, and an upstream
    // body can echo request material including fragments of a key.
    const leak = "sk-not-a-real-key";
    for (const probeClass of [
      "auth",
      "format",
      "quota",
      "endpoint",
      "timeout",
      "unknown",
    ] as ProbeClass[]) {
      expect(describeProbe(probeClass, "Exa").message).not.toContain(leak);
      expect(describeTest(probeClass, "Exa")).not.toContain(leak);
    }
  });

  it("shows a finished test and nothing else", () => {
    expect(testOutcome({ kind: "idle" })).toBeNull();
    expect(testOutcome({ kind: "testing" })).toBeNull();
    expect(testOutcome({ kind: "done", ok: true, message: "ok" })).toEqual({
      tone: "ok",
      message: "ok",
    });
  });
});

describe("destructive confirmations", () => {
  it("asks before every destructive action", () => {
    expect(
      confirmCopy({ kind: "remove", slug: "exa", label: "Exa" }).action,
    ).toBe("Remove");
    expect(
      confirmCopy({ kind: "remove-key", slug: "exa", label: "Exa" }).action,
    ).toBe("Remove key");
    expect(
      confirmCopy({ kind: "disconnect-all", label: "every provider" }).action,
    ).toBe("Disconnect all");
  });

  it("says the key cannot be recovered, because it cannot", () => {
    // A key is write-only and is never shown back, so an operator who clears the
    // wrong one has nothing on screen to retype.
    expect(
      confirmCopy({ kind: "remove", slug: "exa", label: "Exa" }).body,
    ).toContain("never shown back");
    expect(
      confirmCopy({ kind: "remove-key", slug: "exa", label: "Exa" }).body,
    ).toContain("never shown back");
  });

  it("survives being read after its target is cleared", () => {
    // The dialog stays mounted through its own close animation.
    expect(confirmCopy(null).title).toBe("");
  });
});

describe("a class this console has never heard of", () => {
  // `ProbeClass` is a compile-time union and the value is a string off the
  // wire, so a host one version ahead can send a seventh class. Without a
  // fallback the switches fell out returning `undefined` against signatures
  // that say they do not — and `SearchView` reads `.tone` off that inside the
  // `try` wrapping the save, so a save that SUCCEEDED reported an error toast.
  const future = "rate_shaped" as unknown as ProbeClass;

  it("still describes a save", () => {
    const advisory = describeProbe(future, "Brave Search");
    expect(advisory).toBeDefined();
    expect(advisory.tone).toBe("warning");
    // Amber, row kept, key kept: a class this build cannot name is a check
    // whose result it cannot interpret, which is exactly `unknown`.
    expect(advisory.rowCreated).toBe(true);
    expect(advisory.keyKept).toBe(true);
  });

  it("still labels a row and a test", () => {
    expect(healthLabel(future)).toBe("unchecked");
    expect(describeTest(future, "Brave Search")).toBe(
      "The check did not complete.",
    );
  });

  it("has not changed what the six named classes say", () => {
    // The fallback must not have swallowed a case. Auth is the one that
    // matters most: it is the only class that says the key was not kept.
    expect(describeProbe("auth", "Brave Search").keyKept).toBe(false);
    expect(healthLabel("auth")).toBe("key rejected");
    expect(healthLabel("format")).toBe("JSON output off");
  });
});

describe("Set as default", () => {
  it("is not offered on a row that cannot become the default", () => {
    // `resolve::active` requires the marked provider to be COMPLETE and
    // otherwise falls through to the next usable one, so marking an incomplete
    // row changes nothing an agent can feel — while the console answers
    // "Teammates now search through …". Reachable straight after "Remove key",
    // which leaves the row enabled with no credential.
    expect(controlsFor(account({ complete: false }))).not.toContain(
      "make-default",
    );
    expect(
      controlsFor(searxng({ complete: false, endpoint: null })),
    ).not.toContain("make-default");

    // The two conditions that were already right stay right.
    expect(controlsFor(account({ enabled: false }))).not.toContain(
      "make-default",
    );
    expect(controlsFor(account({ isDefault: true }))).not.toContain(
      "make-default",
    );

    // And a complete, enabled, non-default row still gets it — the refusals
    // above are only worth having if the ordinary case survives them.
    expect(controlsFor(account())).toContain("make-default");
  });
});
