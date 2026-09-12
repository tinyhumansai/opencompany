import { describe, expect, it } from "vitest";

import type { ComposioProbeClass } from "@/api/composio";
import {
  advisoryMessage,
  offersSkipVerify,
  probeCopy,
  probeTone,
  storesKey,
  verdictCopy,
  verdictMessage,
} from "@/composio/classify";

/**
 * What the console says about a failed credential check, and what it must never
 * say.
 *
 * The classifier is the host's — the raw upstream error is what it reads, and
 * that string must not reach the console at all. These pin the console's half:
 * which classes kept the key, what each is called, and the one branch that is
 * forbidden from echoing anything the host sent.
 */

const CLASSES: ComposioProbeClass[] = [
  "auth",
  "endpoint",
  "quota",
  "timeout",
  "unknown",
];

/**
 * A plausible upstream body, with the two things that make this dangerous: a
 * request header and a fragment of the key that was just written. Obviously
 * fake — a grep for a leaked credential must have no ambiguous hits here.
 */
const UPSTREAM =
  "401 Unauthorized: x-api-key ak_not_a_real_key_9f3 rejected by edge-proxy-7 (trace 8ab1)";

describe("probeCopy", () => {
  it("has a sentence for every class", () => {
    for (const cls of CLASSES) {
      expect(probeCopy(cls), cls).toBeTruthy();
      expect(probeCopy(cls).length, cls).toBeGreaterThan(10);
    }
  });

  it("says nothing was stored for an auth failure, and Saved for the rest", () => {
    // The half an operator cannot see. A rejected write leaves the page exactly
    // as it was, so without the sentence a refused save and one that silently
    // did nothing are indistinguishable.
    expect(probeCopy("auth")).toContain("Nothing was stored");
    for (const cls of CLASSES.filter((c) => c !== "auth")) {
      expect(probeCopy(cls), cls).toContain("Saved");
    }
  });

  it("gives every class its own sentence", () => {
    expect(new Set(CLASSES.map(probeCopy)).size).toBe(CLASSES.length);
  });

  it("interpolates nothing, because it is handed nothing", () => {
    // Structural rather than incidental: the function takes no text, so there
    // is no upstream string it could echo into a screenshot-able banner.
    expect(probeCopy.length).toBe(1);
    for (const cls of CLASSES) {
      expect(probeCopy(cls), cls).not.toContain("ak_");
    }
  });
});

describe("storesKey / probeTone", () => {
  it("rolls back for auth and only auth", () => {
    // The naive flow rolls everything back on any probe failure, and the naive
    // flow destroys valid credentials: a corporate proxy, a WAF and a rate
    // limit all fail a check while the key is perfectly good.
    expect(storesKey("auth")).toBe(false);
    for (const cls of CLASSES.filter((c) => c !== "auth")) {
      expect(storesKey(cls), cls).toBe(true);
    }
  });

  it("colours red only what actually failed to save", () => {
    expect(probeTone("auth")).toBe("error");
    for (const cls of CLASSES.filter((c) => c !== "auth")) {
      expect(probeTone(cls), cls).toBe("warning");
    }
  });
});

describe("advisoryMessage", () => {
  it("prefers the host's own sentence, which knows what the console does not", () => {
    expect(
      advisoryMessage("quota", "Composio says this account is out of credit."),
    ).toBe("Composio says this account is out of credit.");
  });

  it("falls back to the console's copy when the host sent none", () => {
    expect(advisoryMessage("timeout", undefined)).toBe(probeCopy("timeout"));
    expect(advisoryMessage("timeout", "   ")).toBe(probeCopy("timeout"));
  });

  it("NEVER echoes the upstream string for an unclassified failure", () => {
    // The rule this whole module exists for. `unknown` is by definition the
    // class whose text is the upstream string nobody classified — it can carry
    // request headers and fragments of the key that was just written, and this
    // sentence lands in a banner an operator screenshots into a ticket.
    const message = advisoryMessage("unknown", UPSTREAM);

    expect(message).toBe(probeCopy("unknown"));
    expect(message).not.toContain("ak_");
    expect(message).not.toContain("x-api-key");
    expect(message).not.toContain("edge-proxy-7");
    expect(message).not.toContain("8ab1");
    expect(message).not.toContain(UPSTREAM);
  });

  it("treats a class the host did not send as unknown, not as no failure", () => {
    // A host that sent copy without saying what kind has told us something went
    // wrong and nothing about whether its words are safe to print.
    expect(advisoryMessage(undefined, UPSTREAM)).toBe(probeCopy("unknown"));
  });
});

describe("offersSkipVerify", () => {
  it("offers nothing before an attempt", () => {
    expect(offersSkipVerify(null)).toBe(false);
  });

  it("does not offer to add a key that was already added", () => {
    // An advisory means the write landed. A button offering to add it again
    // would invite a second write of a credential that is already stored.
    expect(
      offersSkipVerify({
        kind: "advisory",
        probeClass: "timeout",
        message: "x",
      }),
    ).toBe(false);
    expect(offersSkipVerify({ kind: "advisory", message: "x" })).toBe(false);
  });

  it("offers it after a refusal the host reached on its own check", () => {
    // Including an `auth` class: a Composio account behind a proxy that
    // rewrites 401s is exactly the operator who cannot otherwise get past a
    // check that is wrong about them.
    expect(
      offersSkipVerify({ kind: "rejected", status: 400, message: "x" }),
    ).toBe(true);
    expect(
      offersSkipVerify({ kind: "rejected", status: 422, message: "x" }),
    ).toBe(true);
    // A transport failure with no status: still worth a retry without the probe.
    expect(offersSkipVerify({ kind: "rejected", message: "x" })).toBe(true);
  });

  it("never offers it after a permission refusal", () => {
    // The viewer may not write this credential at all, and skipping the check
    // would only turn one refusal into two.
    expect(
      offersSkipVerify({ kind: "rejected", status: 401, message: "x" }),
    ).toBe(false);
    expect(
      offersSkipVerify({ kind: "rejected", status: 403, message: "x" }),
    ).toBe(false);
  });
});

/**
 * The check's copy — a separate table from the add path's, because a route that
 * stores nothing must not say it saved.
 *
 * This is a filed defect one surface over (the LLM page's manual Test reuses
 * its add-path advisory copy, so five of six classes open with "Saved" about an
 * event that did not happen). These assertions are what stop someone
 * "deduplicating" the two tables here and reintroducing it.
 */
describe("verdictCopy", () => {
  const CLASSES: ComposioProbeClass[] = [
    "auth",
    "endpoint",
    "quota",
    "timeout",
    "unknown",
  ];

  it("never claims anything was saved", () => {
    for (const cls of CLASSES) {
      expect(verdictCopy(cls), cls).not.toMatch(/saved/i);
    }
  });

  it("says something different from the add path for every class", () => {
    for (const cls of CLASSES) {
      expect(verdictCopy(cls), cls).not.toBe(probeCopy(cls));
      expect(verdictCopy(cls).trim().length, cls).toBeGreaterThan(0);
    }
  });

  it("gives every class its own sentence", () => {
    expect(new Set(CLASSES.map(verdictCopy)).size).toBe(CLASSES.length);
  });
});

describe("verdictMessage", () => {
  it("prefers the host's own sentence", () => {
    expect(verdictMessage("auth", "Composio rejected this key.")).toBe(
      "Composio rejected this key.",
    );
  });

  it("falls back to the console's copy when the host said nothing", () => {
    expect(verdictMessage("timeout", undefined)).toBe(verdictCopy("timeout"));
    expect(verdictMessage("timeout", "   ")).toBe(verdictCopy("timeout"));
  });

  it("never prints the host's text for an unclassified failure", () => {
    // `unknown` is by definition the upstream string nobody classified, and it
    // can echo request headers or a fragment of the key.
    expect(
      verdictMessage("unknown", "x-api-key: ak_not_a_real_key_0123456789"),
    ).toBe(verdictCopy("unknown"));
    expect(
      verdictMessage(undefined, "x-api-key: ak_not_a_real_key_0123456789"),
    ).toBe(verdictCopy("unknown"));
  });
});
