// @vitest-environment jsdom

import { beforeEach, describe, expect, it } from "vitest";

import { scopedKey } from "@/connections/types";
import * as domainModule from "@/lib/domain";
import { isValidDomain, purgeStoredSmtpPasswords } from "@/lib/domain";

/**
 * The SMTP password must never reach `localStorage` (issue #1460).
 *
 * The first half of the fix filtered the password out on the way in: the module
 * kept a `saveMailSettings` that structurally could not receive one. This half
 * removed the store entirely — `Settings → General` reads and writes the host
 * (`src/api/domain.ts`, `src/api/smtp.ts`), so a remembered copy of the domain,
 * host, username or from addresses would only be a second answer that
 * disagrees with the authoritative one.
 *
 * That makes the guarantee a different, stronger shape, so these tests assert a
 * different, stronger thing. It is no longer "the writer drops the password";
 * it is **there is no writer**. `no-writer-left` below is the load-bearing
 * assertion and the one that cannot rot: reintroducing any function that puts
 * something under an `oc-mail` key fails it, whatever that function is called.
 *
 * The assertions about the store are still written against **the whole of
 * `localStorage`** rather than against a known key. A test that checks
 * `oc-mail:…` for a missing `password` field passes the day someone adds a
 * second key, or renames the prefix, or stores a draft somewhere new — and the
 * credential leaks again with a green suite.
 */

const SCOPE = { connection: "conn-a", company: "acme" };
const OTHER_SCOPE = { connection: "conn-b", company: "globex" };
const LEGACY_KEY = "oc-mail:acme";

/** Distinctive enough that a substring search over the store is meaningful. */
const SECRET = "pw-3f9a1c-do-not-persist";

beforeEach(() => {
  localStorage.clear();
});

/** Every value currently in `localStorage`, concatenated. */
function entireStore(): string {
  const parts: string[] = [];
  for (let i = 0; i < localStorage.length; i++) {
    const key = localStorage.key(i);
    if (key === null) continue;
    parts.push(key, localStorage.getItem(key) ?? "");
  }
  return parts.join("\n");
}

/**
 * A blob in the shape the pre-#1460 console wrote, password and all.
 *
 * Built as a literal rather than through a helper from the module, because the
 * helpers that used to build it are exactly what this change deleted. The
 * legacy shape is now test data — it describes what is in an operator's browser
 * today, not anything the console can still produce.
 */
function legacyBlob(password: string = SECRET) {
  return JSON.stringify({
    domain: { domain: "mail.acme.com", verified: false },
    smtp: {
      host: "smtp.postmarkapp.com",
      port: "587",
      security: "starttls",
      username: "apikey",
      password,
      fromName: "Acme",
      fromEmail: "hello@mail.acme.com",
    },
  });
}

/** The same blob a passing purge leaves behind: everything except the password. */
function purgedBlob() {
  return JSON.stringify({
    domain: { domain: "mail.acme.com", verified: false },
    smtp: {
      host: "smtp.postmarkapp.com",
      port: "587",
      security: "starttls",
      username: "apikey",
      fromName: "Acme",
      fromEmail: "hello@mail.acme.com",
    },
  });
}

describe("the store this module used to keep", () => {
  it("has no writer left: nothing exported puts an oc-mail key in localStorage", () => {
    // The guard that replaces the old `@ts-expect-error` on `saveMailSettings`.
    // That one proved a specific function refused a password; this one proves
    // no function is there to refuse. Call every export with plausible
    // arguments and assert the store is untouched — a reintroduced writer under
    // any name fails here.
    const exports = Object.entries(domainModule).filter(
      ([, v]) => typeof v === "function",
    ) as [string, (...args: unknown[]) => unknown][];

    // Sanity: if the module ever exports nothing, the loop below is vacuous and
    // this test would pass while proving nothing.
    expect(exports.length).toBeGreaterThan(0);

    // Two shapes, because the exports do not agree on what an argument is:
    // `isValidDomain` wants a string, a hypothetical writer would want a
    // settings object. A throw is fine and expected — the claim under test is
    // about what reached the store, not about what returned.
    const arguments_ = [
      "mail.acme.com",
      {
        connection: "conn-a",
        company: "acme",
        domain: { domain: "mail.acme.com", verified: false },
        smtp: { host: "smtp.postmarkapp.com", password: SECRET },
      },
    ];

    for (const [, fn] of exports) {
      for (const argument of arguments_) {
        try {
          fn(argument);
        } catch {
          /* wrong argument type for this export; the store assertion still holds */
        }
      }
    }

    expect(entireStore()).not.toContain(SECRET);
    expect(entireStore()).not.toContain("oc-mail");
  });

  it("exports only the domain pre-flight and the one-shot purge", () => {
    // Names the surface, so deleting the store cannot quietly grow back a
    // `loadMailSettings` that a reviewer skims past.
    expect(Object.keys(domainModule).sort()).toEqual([
      "isValidDomain",
      "purgeStoredSmtpPasswords",
    ]);
  });
});

describe("purging what the old console already stored", () => {
  it("clears passwords from every scope, not just the one on screen", () => {
    localStorage.setItem(scopedKey("oc-mail", SCOPE), legacyBlob());
    localStorage.setItem(scopedKey("oc-mail", OTHER_SCOPE), legacyBlob());
    localStorage.setItem(LEGACY_KEY, legacyBlob());

    expect(purgeStoredSmtpPasswords()).toBe(3);
    expect(entireStore()).not.toContain(SECRET);
  });

  it("keeps the operator's non-secret work readable", () => {
    // The store is retired, so nothing reads these back into the form any
    // more — but deleting an operator's typed-in host and from address on
    // their behalf is not this function's job either. It removes the
    // credential and leaves the rest alone.
    localStorage.setItem(scopedKey("oc-mail", SCOPE), legacyBlob());

    purgeStoredSmtpPasswords();

    expect(localStorage.getItem(scopedKey("oc-mail", SCOPE))).toBe(purgedBlob());
  });

  it("strips a password nested somewhere the old shape never put one", () => {
    // Intermediate builds and hand-edited blobs both exist. The contract is
    // about the whole value, so the strip is about the whole value.
    localStorage.setItem(
      scopedKey("oc-mail", SCOPE),
      JSON.stringify({ drafts: [{ smtp: { auth: { password: SECRET } } }] }),
    );

    expect(purgeStoredSmtpPasswords()).toBe(1);
    expect(entireStore()).not.toContain(SECRET);
  });

  it("removes a key whose JSON cannot be parsed but mentions a password", () => {
    // Cannot be shown to be clean, so it cannot be left in place.
    localStorage.setItem(scopedKey("oc-mail", SCOPE), `{"smtp":{"password":"${SECRET}"`);

    expect(purgeStoredSmtpPasswords()).toBe(1);
    expect(localStorage.getItem(scopedKey("oc-mail", SCOPE))).toBeNull();
    expect(entireStore()).not.toContain(SECRET);
  });

  it("leaves unrelated keys alone and reports nothing to clean", () => {
    localStorage.setItem("oc-tour:conn-a::acme", '{"step":3}');
    localStorage.setItem(scopedKey("oc-mail", SCOPE), purgedBlob());

    expect(purgeStoredSmtpPasswords()).toBe(0);
    expect(localStorage.getItem("oc-tour:conn-a::acme")).toBe('{"step":3}');
    expect(localStorage.getItem(scopedKey("oc-mail", SCOPE))).toBe(purgedBlob());
  });

  it("is idempotent", () => {
    localStorage.setItem(scopedKey("oc-mail", SCOPE), legacyBlob());

    expect(purgeStoredSmtpPasswords()).toBe(1);
    expect(purgeStoredSmtpPasswords()).toBe(0);
    expect(entireStore()).not.toContain(SECRET);
  });
});

describe("the domain pre-flight", () => {
  // UX, not a guard — the host does not validate — so what matters is only that
  // an operator who typed something unusable hears about it before a round
  // trip, and that a real domain is never refused.
  it("accepts a hostname with at least one dot", () => {
    expect(isValidDomain("mail.acme.com")).toBe(true);
    expect(isValidDomain("acme.co")).toBe(true);
    expect(isValidDomain("  mail.acme.com  ")).toBe(true);
  });

  it("rejects a bare label, an empty string, and a leading hyphen", () => {
    expect(isValidDomain("acme")).toBe(false);
    expect(isValidDomain("")).toBe(false);
    expect(isValidDomain("   ")).toBe(false);
    expect(isValidDomain("-acme.com")).toBe(false);
  });
});
