import { describe, expect, it } from "vitest";

import type { CompanyBilling, CompanyCredentialStatus } from "@/api/credential";
import {
  accountShape,
  accountSubline,
  balanceLine,
  canRemoveKey,
  headerAction,
  REMOVAL_CONSEQUENCE,
  REMOVAL_AND_THINKING,
} from "@/views/connections/account";

function status(overrides: Partial<CompanyCredentialStatus> = {}): CompanyCredentialStatus {
  return {
    configured: true,
    source: "company",
    notice: "notice",
    hubLink: false,
    ...overrides,
  };
}

describe("accountShape keeps an unreadable store apart from an empty one", () => {
  it("is connected whenever anything resolves", () => {
    expect(accountShape("ready", status({ source: "company" }))).toBe("connected");
    expect(accountShape("ready", status({ configured: false, source: "attested" }))).toBe(
      "connected",
    );
    expect(accountShape("ready", status({ configured: false, source: "static" }))).toBe(
      "connected",
    );
  });

  it("is empty only when the chain resolves to nothing", () => {
    expect(accountShape("ready", status({ configured: false, source: "none" }))).toBe("empty");
  });

  // The trap the whole page is built around. `company_key::resolve` propagates
  // a store read error rather than falling through, because a connection made
  // under a silently-borrowed identity belongs to the wrong account invisibly
  // and permanently. A console that folded that into "empty" would throw the
  // distinction away at the last step and send an admin to set a key they may
  // already have set.
  it("is unknown — never empty — when the host could not answer", () => {
    expect(accountShape("error", null)).toBe("unknown");
    // Even holding a status from a previous good read: the load says the
    // current answer is not known, and that outranks a stale one.
    expect(accountShape("error", status({ source: "company" }))).toBe("unknown");
  });
});

describe("accountSubline says which tier actually answers", () => {
  it("names the company's own account", () => {
    expect(accountSubline("ready", status({ source: "company" }))).toContain(
      "this company's own TinyHumans account",
    );
  });

  // The hosted case. `configured` is false here and a row built on it would
  // read "not configured" while the server's identity is the one answering.
  it("names the server's account for both fallback identities", () => {
    for (const source of ["attested", "static"] as const) {
      expect(accountSubline("ready", status({ configured: false, source }))).toBe(
        "Acting as the account of whoever runs this server",
      );
    }
  });

  // `source` comes from `company_key::resolve` and reports which TinyHumans
  // identity won — nothing about who pays for thinking, which `inference/config`
  // and `inference/key` decide independently on the LLM page. A company on
  // `source: "company"` can be thinking on its own OpenRouter key; one on the
  // instance's identity can be paying a provider direct. A payer named from
  // this value would send an operator chasing spend to the wrong account.
  it("claims no payer, in any state it can reach", () => {
    for (const source of ["company", "attested", "static", "none"] as const) {
      const line = accountSubline("ready", status({ source }));
      expect(line.toLowerCase()).not.toContain("billed");
      expect(line.toLowerCase()).not.toContain("pays");
      expect(line.toLowerCase()).not.toContain("paying");
    }
    expect(accountSubline("loading", null).toLowerCase()).not.toContain("billed");
    expect(accountSubline("error", null).toLowerCase()).not.toContain("billed");
  });

  // Narrow on purpose. "Agents cannot think" is what this line said first, and
  // it is **false** on a company whose LLM page holds a provider key of its
  // own — `inference/key` resolves without this credential. The sub-line states
  // the absence; the empty state carries the consequence with its exception
  // named.
  it("states the absence without claiming the company has stopped", () => {
    const line = accountSubline("ready", status({ configured: false, source: "none" }));
    expect(line).toBe("No TinyHumans account for this company");
  });

  it("never claims agents cannot think, in any state", () => {
    for (const load of ["ready", "error", "loading"] as const) {
      for (const source of ["company", "attested", "static", "none"] as const) {
        const line = accountSubline(load, status({ source })).toLowerCase();
        expect(line, `${load}/${source}`).not.toContain("cannot think");
      }
    }
  });

  it("does not claim there is no key when the host could not answer", () => {
    const line = accountSubline("error", null);
    expect(line).toContain("not the same as having no key");
    expect(line).not.toContain("No TinyHumans account");
  });

  it("falls back to what the row is when a host names an unknown tier", () => {
    const unknown = status({ source: "something-new" as CompanyCredentialStatus["source"] });
    expect(accountSubline("ready", unknown)).toBe(
      "The account this company acts and spends through",
    );
  });
});

describe("canRemoveKey offers Remove only where it would remove something", () => {
  it("is true for the company's own key", () => {
    expect(canRemoveKey(status({ source: "company" }))).toBe(true);
  });

  // The instance's identity is not this row's to take away, and a Remove that
  // clears nothing is the control-that-cannot-act the LLM page's pass deleted
  // a toggle over.
  it("is false for a fallback identity, for nothing, and for an unknown state", () => {
    expect(canRemoveKey(status({ configured: false, source: "attested" }))).toBe(false);
    expect(canRemoveKey(status({ configured: false, source: "static" }))).toBe(false);
    expect(canRemoveKey(status({ configured: false, source: "none" }))).toBe(false);
    expect(canRemoveKey(null)).toBe(false);
  });
});

describe("headerAction holds whichever action is live", () => {
  it("offers nothing to a member", () => {
    expect(headerAction(status({ hubLink: true }), false)).toBeNull();
    expect(headerAction(status({ hubLink: false }), false)).toBeNull();
  });

  it("prefers the grant where the host has a hub", () => {
    expect(headerAction(status({ hubLink: true }), true)).toBe("connect");
  });

  // Without this the header card on a self-hosted instance is a heading over
  // empty space: `ConnectTinyHumansButton` renders null with no hub wired.
  it("falls back to the paste dialog where it does not", () => {
    expect(headerAction(status({ hubLink: false }), true)).toBe("key");
    expect(headerAction(status({ hubLink: undefined }), true)).toBe("key");
  });

  // A null status is the read having failed or not yet landed — not "no key".
  // The row beside this button says so in words, and an enabled "Add a key"
  // under that sentence opens a blind overwrite of a write-only credential the
  // console has just admitted it cannot see. The value it would replace cannot
  // be read back from anywhere, which is what makes this worse than an ordinary
  // control-that-cannot-act.
  it("offers nothing at all while the credential state is unknown", () => {
    expect(headerAction(null, true)).toBeNull();
    expect(headerAction(null, false)).toBeNull();
  });
});

describe("balanceLine", () => {
  function billing(overrides: Partial<CompanyBilling> = {}): CompanyBilling {
    return { configured: true, ...overrides };
  }

  it("renders no row for a company with no account of its own", () => {
    expect(balanceLine(null)).toBeNull();
    expect(balanceLine(billing({ configured: false }))).toBeNull();
  });

  it("shows the figure and the plan", () => {
    const line = balanceLine(
      billing({ summary: { balanceUsd: 12.5, plan: "pro", activeSubscription: true } }),
    );
    expect(line?.amount).toBe("$12.50");
    expect(line?.detail).toBe("on the pro plan · subscription active");
    expect(line?.low).toBe(false);
  });

  // `!balanceUsd` would hide exactly the figure somebody needs to see, and a
  // zero balance is the state the page exists to make loud.
  it("shows zero rather than hiding it, and marks it low", () => {
    const line = balanceLine(
      billing({ summary: { balanceUsd: 0, plan: "free", activeSubscription: false } }),
    );
    expect(line?.amount).toBe("$0.00");
    expect(line?.low).toBe(true);
  });

  // "We could not ask" and "there is nothing left" look identical on a row and
  // call for opposite actions, so the figure is dropped rather than invented.
  it("does not render an unanswered hub as a zero balance", () => {
    const line = balanceLine(billing({ unavailable: "the hub timed out" }));
    expect(line?.amount).toBeNull();
    expect(line?.low).toBe(false);
    expect(line?.detail).toContain("the hub timed out");
    expect(line?.detail).toContain("The key is set");
  });
});

describe("the removal confirmation says only what removal does", () => {
  // `set_key("")` clears `tinyhumans/key` and stops. `finish_link` writes the
  // granted value into `inference/key` as well and declares the `managed`
  // provider, so on a company that took the one-click path — the path the
  // button beside this dialog recommends — every agent turn is still billed to
  // this account after the key is removed. The old sentence promised the
  // opposite, on the one screen where being wrong costs a credential.
  it("never claims the billing stops", () => {
    const all = `${REMOVAL_CONSEQUENCE} ${REMOVAL_AND_THINKING}`;
    expect(all).not.toContain("stops being billed");
  });

  // The sentence that has now been wrong in both directions. Before #2266 the
  // grant copied the key into `inference/key`, so removal here left a second
  // copy thinking on the same account; it copies nothing now, and a managed
  // turn resolves through `tinyhumans/key` itself. So the honest sentence is
  // the fallback the resolver actually takes, not a reassurance — and not the
  // opposite overclaim either, since a TinyHumans key on the LLM page outranks
  // this one and goes on working.
  it("describes what the resolver does next, not a reassurance", () => {
    expect(REMOVAL_AND_THINKING).toContain("resolve through this same key");
    expect(REMOVAL_AND_THINKING).toContain("whoever runs this server");
    expect(REMOVAL_AND_THINKING).toContain("nothing at all if this instance carries none");
    expect(REMOVAL_AND_THINKING).toContain("outranks this one and keeps working");
    // The claim the old copy made, which the merged inference rework falsified.
    expect(REMOVAL_AND_THINKING).not.toContain("puts the same key on the LLM page");
  });

  // Both fallbacks, because `GET …/credential` reports the tier that won and
  // that is `company` whichever way it went. Naming one would be a guess
  // dressed as a fact.
  it("offers both fallbacks rather than guessing which applies", () => {
    expect(REMOVAL_CONSEQUENCE).toContain("whoever runs this server");
    expect(REMOVAL_CONSEQUENCE).toContain("no account at all");
  });

  // Conditional at the top, because the outcome is: only a company whose
  // models are set to TinyHumans is standing on this key at all.
  it("states the thinking half as the conditional it is", () => {
    expect(REMOVAL_AND_THINKING).toContain("where this company's models are set to TinyHumans");
  });
});
