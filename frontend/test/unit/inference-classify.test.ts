import { describe, expect, it } from "vitest";

import {
  checkOutcome,
  describeProbe,
  destroysCredential,
  healthLabel,
  offersAddAnyway,
  testOutcome,
} from "@/inference/classify";
import type { ProbeClass } from "@/inference/types";

const ALL_CLASSES: ProbeClass[] = ["auth", "model", "quota", "endpoint", "timeout", "unknown"];

/**
 * What the console says about a failed provider check.
 *
 * The classifier itself is not on this side — the host decides the class and
 * sends it, because the raw upstream error is what the classifier reads and that
 * string must never reach the console. What is here is copy selection, and it is
 * a plain function precisely so these cases are reachable without rendering a
 * banner.
 */
describe("presenting a failed provider check", () => {
  it("treats exactly one class as destructive", () => {
    // The naive add flow rolls everything back on any probe failure, and the
    // naive flow destroys valid credentials: a proxy, a WAF, a rate limit and a
    // mistyped model id all fail a probe while the key is perfectly good.
    expect(ALL_CLASSES.filter(destroysCredential)).toEqual(["auth"]);
  });

  it("colours the five non-destructive classes amber, not red", () => {
    // The save succeeded. Only reachability is in question, and calling that an
    // error is a lie about what happened.
    for (const probeClass of ALL_CLASSES.filter((c) => c !== "auth")) {
      const advisory = describeProbe(probeClass, "Acme");
      expect(advisory.tone).toBe("warning");
      expect(advisory.rowCreated).toBe(true);
      expect(advisory.keyKept).toBe(true);
      expect(advisory.message.startsWith("Saved")).toBe(true);
    }
  });

  it("creates no row and keeps no key for a rejected credential", () => {
    const advisory = describeProbe("auth", "Acme");
    expect(advisory.tone).toBe("error");
    expect(advisory.rowCreated).toBe(false);
    expect(advisory.keyKept).toBe(false);
    expect(advisory.message).toBe("Could not reach Acme: the provider rejected the credential.");
  });

  it("names the provider only where naming it is the next step", () => {
    expect(describeProbe("endpoint", "api.acme.dev").message).toContain("api.acme.dev");
    expect(describeProbe("timeout", "api.acme.dev").message).toContain("api.acme.dev");
    // These are about the account or the check, not about a specific address.
    expect(describeProbe("quota", "api.acme.dev").message).not.toContain("api.acme.dev");
    expect(describeProbe("model", "api.acme.dev").message).not.toContain("api.acme.dev");
  });

  it("gives every class a sentence", () => {
    for (const probeClass of ALL_CLASSES) {
      expect(describeProbe(probeClass, "Acme").message.length).toBeGreaterThan(10);
    }
  });
});

describe("what a connected row shows beside its endpoint", () => {
  it("names the state for its remedy, not for its status code", () => {
    expect(healthLabel("ok")).toBe("ok");
    expect(healthLabel("auth")).toBe("key rejected");
    expect(healthLabel("endpoint")).toBe("unreachable");
    expect(healthLabel("quota")).toBe("out of credit");
  });

  it("has a label for every class a probe can produce", () => {
    for (const probeClass of ALL_CLASSES) {
      expect(healthLabel(probeClass)).toBeTruthy();
    }
  });
});

describe("offering to add without verifying", () => {
  it("unlocks only on a typed probe failure", () => {
    // A provider that serves no OpenAI-shaped `/models` listing is still usable
    // for inference, so blocking creation leaves those operators unable to reach
    // the model field at all.
    expect(offersAddAnyway({ kind: "probe", probeClass: "endpoint" })).toBe(true);
  });

  it("stays locked for a failure that is not evidence about the endpoint", () => {
    // A slug collision or a failed key write says nothing about whether the
    // endpoint is fine, so neither may unlock "add anyway".
    expect(offersAddAnyway({ kind: "slugCollision" })).toBe(false);
    expect(offersAddAnyway({ kind: "keyWriteFailed" })).toBe(false);
  });
});

describe("what a finished test reads as on a row", () => {
  it("has a success tone of its own", () => {
    // `AdvisoryTone` has no success case, deliberately: it describes what
    // happened to a *save*, where the only question is how bad the news is. A
    // test can simply be good news.
    expect(testOutcome({ kind: "done", ok: true, message: "Reached the provider." })).toEqual({
      tone: "ok",
      message: "Reached the provider.",
    });
  });

  it("keeps the host's own sentence rather than collapsing to 'failed'", () => {
    // A 407 behind a corporate proxy has to read differently from a rejected
    // key — that distinction is the whole reason the classifier exists, and
    // this is the last step where it could be thrown away.
    const proxy = testOutcome({
      kind: "done",
      ok: false,
      message: "Saved, but the check did not complete.",
    });
    const auth = testOutcome({
      kind: "done",
      ok: false,
      message: "Could not reach Acme: the provider rejected the credential.",
    });
    expect(proxy?.message).not.toBe(auth?.message);
    expect(proxy?.tone).toBe("error");
  });

  it("shows nothing while idle or in flight", () => {
    expect(testOutcome({ kind: "idle" })).toBeNull();
    expect(testOutcome({ kind: "testing" })).toBeNull();
  });
});

describe("checkOutcome", () => {
  it("does not call a bogus model id a success", () => {
    // The whole defect: the control answered "is the endpoint reachable" while
    // the console asked "will this model answer", so `this-model-does-not-exist`
    // came back as "Reached the provider."
    const out = checkOutcome({ ok: true, modelCount: 51, modelKnown: false });
    expect(out.message).toContain("does not publish that model id");
  });

  it("reports a listed model as settled", () => {
    expect(checkOutcome({ ok: true, modelCount: 51, modelKnown: true }).message).toContain(
      "publishes that model",
    );
  });

  it("treats an unpublished id as a caution rather than a failure", () => {
    // An Azure deployment name is never in `/models` by design. Failing it
    // would make the only correct value at that endpoint unreachable.
    expect(checkOutcome({ ok: true, modelCount: 51, modelKnown: false }).ok).toBe(true);
  });

  it("says nothing about a model when none was asked about", () => {
    const out = checkOutcome({ ok: true, modelCount: 51 });
    expect(out).toEqual({ ok: true, message: "Reached the provider." });
  });

  it("keeps the host's own sentence on a failure", () => {
    // A 407 behind a corporate proxy has to read differently from a rejected
    // key — collapsing it to "failed" throws that away at the last step.
    const out = checkOutcome({ ok: false, message: "A proxy refused the request.", modelCount: 0 });
    expect(out).toEqual({ ok: false, message: "A proxy refused the request." });
  });
});
