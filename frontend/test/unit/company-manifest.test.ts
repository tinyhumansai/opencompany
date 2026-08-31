import { afterEach, describe, expect, it, vi } from "vitest";

import { ApiError } from "@/api/types";
import {
  buildManifestToml,
  collidesWithArchived,
  describeProvisionError,
  explicitIdProblem,
  resetReplacementId,
  walletAddressProblem,
  wasAmbiguousProvisionOutcome,
} from "@/lib/company-manifest";

/**
 * The pure half of console company creation (issue #1807).
 *
 * `buildManifestToml` is what stands between an operator's typed name and a body
 * the host will accept, so its whole job is to be a *valid, minimal* manifest —
 * name always, the two optional sections only when the operator gave a value.
 * `describeProvisionError` is what turns a refused provision into a sentence,
 * and the one that matters is `company_exists`: the host names an id the
 * operator never typed, so the console has to re-word it.
 */
describe("buildManifestToml (issue #1807)", () => {
  it("builds a valid minimal manifest from a name alone", () => {
    const toml = buildManifestToml({ name: "Acme Robotics" });
    expect(toml).toBe('[company]\nname = "Acme Robotics"\n');
    // The host injects policy.mode and users.mode for an omitted section, so
    // neither belongs in the minimal body.
    expect(toml).not.toContain("[users]");
    expect(toml).not.toContain("[policy]");
  });

  it("escapes quotes and backslashes in the name so the TOML stays valid", () => {
    const toml = buildManifestToml({ name: 'A "quoted" C:\\orp' });
    expect(toml).toContain('name = "A \\"quoted\\" C:\\\\orp"');
  });

  /**
   * Codex review on #1828 (PR comment 3865689246): TOML's `basic-unescaped`
   * grammar excludes U+007F (DEL) — it falls outside both printable-ASCII
   * ranges (`%x23-5B` / `%x5D-7E`) the spec allows literal — but the old
   * condition only escaped code points below U+0020, so a name containing a
   * pasted DEL byte produced a manifest the host's TOML parser refuses. On a
   * reset that surfaces only after the old company is already archived.
   */
  it("escapes a DEL (U+007F) in the name so the host's TOML parser accepts it", () => {
    const toml = buildManifestToml({ name: "Acme\u007fRobotics" });
    expect(toml).toContain('name = "Acme\\u007fRobotics"');
    // eslint-disable-next-line no-control-regex -- asserting the raw byte is gone
    expect(toml).not.toMatch(/\u007f/);
  });

  it("leaves other C1-range and extended Unicode characters literal, unescaped", () => {
    // U+0080 is the first `non-ascii` code point TOML allows literal in a
    // basic string — confirms the fix didn't widen the escape past DEL.
    const toml = buildManifestToml({ name: "Acme\u0080Robotics" });
    expect(toml).toContain("name = \"Acme\u0080Robotics\"");
  });

  it("writes [users].admins only when an admin email is given", () => {
    expect(buildManifestToml({ name: "Acme" })).not.toContain("admins");

    const withAdmin = buildManifestToml({ name: "Acme", adminEmail: "ceo@acme.test" });
    expect(withAdmin).toContain("[users]");
    expect(withAdmin).toContain('admins = ["ceo@acme.test"]');
  });

  it("ignores a blank admin email rather than emitting an empty admins list", () => {
    const toml = buildManifestToml({ name: "Acme", adminEmail: "   " });
    expect(toml).not.toContain("[users]");
    expect(toml).not.toContain("admins");
  });

  it("writes [policy].mode only when a tier was chosen", () => {
    expect(buildManifestToml({ name: "Acme" })).not.toContain("[policy]");

    const supervised = buildManifestToml({ name: "Acme", policyMode: "supervised" });
    expect(supervised).toContain("[policy]");
    expect(supervised).toContain('mode = "supervised"');
  });

  it("emits wallet mode with mode = \"wallet\" and a wallets list when wallets are given", () => {
    const toml = buildManifestToml({
      name: "Acme",
      wallets: ["11111111111111111111111111111111"],
    });
    expect(toml).toContain("[users]");
    // The host's manifest validator only reads `[users].wallets` when the
    // manifest itself declares wallet mode.
    expect(toml).toContain('mode = "wallet"');
    expect(toml).toContain('wallets = ["11111111111111111111111111111111"]');
  });

  it("prefers wallets over an admin email so the two lists never share a [users] block", () => {
    const toml = buildManifestToml({
      name: "Acme",
      adminEmail: "ceo@acme.test",
      wallets: ["11111111111111111111111111111111", "22222222222222222222222222222222"],
    });
    // Wallet mode never reads `admins`, and the validator refuses `admins`
    // alongside wallet mode — so only the wallets are emitted.
    expect(toml).toContain('mode = "wallet"');
    expect(toml).toContain(
      'wallets = ["11111111111111111111111111111111", "22222222222222222222222222222222"]',
    );
    expect(toml).not.toContain("admins");
    expect(toml).not.toContain("ceo@acme.test");
  });

  it("ignores blank wallet entries and falls back to email mode when none survive", () => {
    const toml = buildManifestToml({
      name: "Acme",
      adminEmail: "ceo@acme.test",
      wallets: ["   ", ""],
    });
    expect(toml).not.toContain("wallet");
    expect(toml).toContain('admins = ["ceo@acme.test"]');
  });
});

describe("walletAddressProblem", () => {
  it("rejects a blank address", () => {
    expect(walletAddressProblem("   ")).toMatch(/wallet address/i);
  });

  it("rejects non-base58 characters", () => {
    // `0`, `O`, `I`, `l` are not in the base58 alphabet.
    expect(walletAddressProblem("0OIl0OIl0OIl0OIl0OIl0OIl0OIl0OIl")).toMatch(/base58/i);
  });

  it("rejects an implausibly short address", () => {
    expect(walletAddressProblem("abc")).toMatch(/32-byte/i);
  });

  it("accepts a plausible base58 address", () => {
    expect(walletAddressProblem("11111111111111111111111111111111")).toBeNull();
  });

  it("rejects a same-length string that decodes to the wrong byte count (codex review on #1943, PR comment 3894416376)", () => {
    // 32 `z` characters is within the old length-only bound (32-48) and every
    // character is valid base58, but base58 is not fixed-width per character:
    // this string decodes to 24 bytes, not 32 — which is exactly what the
    // host's `decode_wallet_address` would refuse. A length-only check passed
    // this and let a reset archive the old company before discovering
    // provisioning could never have succeeded.
    const address = "z".repeat(32);
    const problem = walletAddressProblem(address);
    expect(problem).not.toBeNull();
    expect(problem).toMatch(/32-byte/i);
  });
});

describe("describeProvisionError (issue #1807)", () => {
  it("re-words company_exists in the operator's terms (they typed a name, not an id)", () => {
    const msg = describeProvisionError(
      new ApiError(409, "company_exists", "company already exists: acme", true),
    );
    expect(msg).toContain("already exists");
    expect(msg).toContain("different name");
  });

  it("shows the host's quota message verbatim", () => {
    const host = "tenant company quota of 5 reached";
    expect(
      describeProvisionError(new ApiError(429, "quota_exceeded", host, true)),
    ).toBe(host);
  });

  it("explains a platform-scope refusal in terms of the sign-in", () => {
    const msg = describeProvisionError(new ApiError(401, "unauthorized", "unauthorized", true));
    expect(msg).toContain("platform credential");
  });

  it("falls back to a generic line for a non-ApiError", () => {
    expect(describeProvisionError(new Error("boom"))).toContain("Something went wrong");
  });
});

/**
 * Codex review on #1828 (PR comment 3862711330): a shared-single-DB host
 * namespaces every provisioned id with `<tenant>--`, invisibly to the
 * console. An archived company's `CompanyStatus.id` is therefore the
 * namespaced form (e.g. `tenant-a--acme`), and the dialog's earlier
 * archived-id guard only rejected an exact string match against that full
 * id — so a bare `acme` typed into Advanced sailed through the check and
 * was re-namespaced back to `tenant-a--acme` by the host, recreating the
 * exact collision the guard exists to prevent.
 */
describe("collidesWithArchived (issue #1807)", () => {
  it("catches an exact match against the archived id", () => {
    expect(collidesWithArchived("acme", "acme")).toBe(true);
  });

  it("catches the bare id under a tenant-namespaced archived id", () => {
    expect(collidesWithArchived("acme", "tenant-a--acme")).toBe(true);
  });

  it("does not flag a genuinely distinct id", () => {
    expect(collidesWithArchived("acme-mk2", "acme")).toBe(false);
    expect(collidesWithArchived("acme", "tenant-a--other")).toBe(false);
  });

  it("does not flag the tenant-namespaced form itself as if it were bare", () => {
    // Only the bare tail collides; the full namespaced string typed back in
    // is already caught by the exact-match branch above, not this one.
    expect(collidesWithArchived("tenant-a--acme", "tenant-a--acme")).toBe(true);
    expect(collidesWithArchived("tenant-b--acme", "tenant-a--acme")).toBe(false);
  });
});

/**
 * `explicitIdProblem` only checked an operator-typed id's length and the two
 * reserved dot-segments — anything else was accepted, even though `slug`
 * (`store/paths.rs`) does not pass every character through unmodified.
 * `Bundle::new` derives a company's on-disk directory from `slug(id)`, and
 * `FsCompanyStore::list` reconstructs a company's id FROM that directory
 * name on every subsequent read (never from anything stored inside the
 * bundle) — so an id `slug` would silently change, like `acme corp` →
 * `acme_corp`, provisions and works for the request that created it, then
 * comes back under the changed id after any restart (codex review on
 * #1828, PR comment 3875297936).
 */
describe("explicitIdProblem — slug-stability (issue #1828 comment 3875297936)", () => {
  it("accepts every character slug passes through unmodified", () => {
    expect(explicitIdProblem("acme-corp_2.mk2")).toBeNull();
    expect(explicitIdProblem("ACME123")).toBeNull();
  });

  it("rejects a space, which slug silently folds to _", () => {
    const problem = explicitIdProblem("acme corp");
    expect(problem).not.toBeNull();
    expect(problem).toContain("letters, numbers");
  });

  it("rejects a slash, which slug silently folds to _", () => {
    const problem = explicitIdProblem("acme/ops");
    expect(problem).not.toBeNull();
    expect(problem).toContain("letters, numbers");
  });

  it("still rejects the reserved dot-segments ahead of the charset check", () => {
    // "." and ".." are themselves entirely slug-safe characters, so they
    // need their own, more specific message rather than falling through to
    // the generic charset one.
    expect(explicitIdProblem(".")).toContain("reserved path segment");
    expect(explicitIdProblem("..")).toContain("reserved path segment");
  });

  it("still enforces the length bound ahead of the charset check", () => {
    const tooLong = "a".repeat(129);
    expect(explicitIdProblem(tooLong)).toContain("too long");
  });
});

/**
 * Codex review on #1828 (PR comment 3875745309): `slug` (`store/paths.rs`)
 * allowlists `.` and passes it through unmodified at any position, so
 * `slug("acme.") === "acme."` and the slug-stability check above cannot see
 * a trailing period as a problem. Windows Win32 path handling strips a
 * trailing period from a path component before the directory is ever
 * created — the same hazard already documented and defended against for
 * secret filenames (`percent_encode`, `store/paths.rs`) — so on a
 * Windows-backed host `acme.` is created on disk as `acme`, and `list`
 * reconstructs the id from that directory name on the next read: the
 * bundle is created under `acme.` and comes back as `acme` after a restart.
 */
describe("explicitIdProblem — trailing period (issue #1828 comment 3875745309)", () => {
  it("rejects an id ending in a period, which Windows strips from the folder name", () => {
    const problem = explicitIdProblem("acme.");
    expect(problem).not.toBeNull();
    expect(problem).toContain("end with a period");
  });

  it("still rejects the reserved dot-segments ahead of the trailing-period check", () => {
    // "." and ".." both end in a period too, but they need their own,
    // more specific "reserved path segment" message.
    expect(explicitIdProblem(".")).toContain("reserved path segment");
    expect(explicitIdProblem("..")).toContain("reserved path segment");
  });

  it("accepts an interior period — only a trailing one is a Windows hazard", () => {
    expect(explicitIdProblem("acme.corp")).toBeNull();
  });
});

/**
 * `slug` (`store/paths.rs`) allowlists letters, digits, `.`, `_`, and `-`, and
 * folds everything else to `_` — a Windows reserved device name like `con` is
 * already entirely within that charset, so none of the earlier checks (length,
 * dot-segments, trailing period, charset) catch it. Win32 reserves `CON`,
 * `PRN`, `AUX`, `NUL`, `COM1`-`COM9`, and `LPT1`-`LPT9` as device names —
 * case-insensitively, and regardless of any extension appended after them,
 * because the reservation is on the base name, not the full filename — so
 * `FsCompanyStore` can't create the corresponding company directory on a
 * Windows-backed host at all. Caught here, before the archive leg runs, for
 * the same reason the other checks in this file are: an id this client could
 * have refused for free is otherwise discovered only after the old company
 * has already been archived (codex review on #1828, PR comment 3876096427).
 */
describe("explicitIdProblem — Windows reserved device names (issue #1828 comment 3876096427)", () => {
  it("rejects a bare reserved device name", () => {
    const problem = explicitIdProblem("con");
    expect(problem).not.toBeNull();
    expect(problem).toContain("reserved");
  });

  it("rejects reserved names case-insensitively", () => {
    expect(explicitIdProblem("CON")).toContain("reserved");
    expect(explicitIdProblem("Aux")).toContain("reserved");
    expect(explicitIdProblem("Com1")).toContain("reserved");
  });

  it("rejects a reserved name with an extension appended", () => {
    // Win32 reserves the device name itself, not the full filename — slug's
    // "." passthrough doesn't help here.
    expect(explicitIdProblem("con.txt")).toContain("reserved");
    expect(explicitIdProblem("lpt1.log")).toContain("reserved");
  });

  it("covers every reserved base name", () => {
    for (const name of ["CON", "PRN", "AUX", "NUL", "COM1", "COM9", "LPT1", "LPT9"]) {
      expect(explicitIdProblem(name)).toContain("reserved");
    }
  });

  it("does not reject a name that merely starts with a reserved prefix", () => {
    // "console" and "comet" aren't reserved — only the exact device name
    // (optionally followed by an extension) is.
    expect(explicitIdProblem("console")).toBeNull();
    expect(explicitIdProblem("comet")).toBeNull();
    expect(explicitIdProblem("auxiliary")).toBeNull();
  });

  it("does not reject COM0 or LPT0, which are not reserved", () => {
    expect(explicitIdProblem("com0")).toBeNull();
    expect(explicitIdProblem("lpt0")).toBeNull();
  });
});

/**
 * Codex review on #1828, PR comment 3878583836: `register_and_report_status`
 * (`server/provision.rs`) registers the company — live in the registry and
 * owned — BEFORE reading its status back for the response, so a store
 * failure in that read-back still leaves the company live. That failure
 * reaches this client as `store_error`, the same code a store failure
 * inside `RuntimeBuilder::build` produces — one that ran BEFORE
 * registration, where the company genuinely was never created. The two are
 * indistinguishable by code alone, which is exactly the ambiguity
 * `network_error` already has here, so `store_error` belongs in the same
 * reconciled set.
 */
describe("wasAmbiguousProvisionOutcome — store_error (issue #1828 comment 3878583836)", () => {
  it("treats a post-registration store_error as ambiguous, worth reconciling", () => {
    const err = new ApiError(
      500,
      "store_error",
      "could not read the company's status after creating it",
      true,
    );
    expect(wasAmbiguousProvisionOutcome(err)).toBe(true);
  });

  it("still treats network_error and company_exists as ambiguous", () => {
    expect(
      wasAmbiguousProvisionOutcome(new ApiError(0, "network_error", "network error", true)),
    ).toBe(true);
    expect(
      wasAmbiguousProvisionOutcome(
        new ApiError(409, "company_exists", "company already exists: acme", true),
      ),
    ).toBe(true);
  });

  it("does not treat an ordinary, unambiguous refusal as worth reconciling", () => {
    const err = new ApiError(429, "quota_exceeded", "tenant company quota of 3 reached", true);
    expect(wasAmbiguousProvisionOutcome(err)).toBe(false);
  });

  it("does not treat a non-ApiError as ambiguous", () => {
    expect(wasAmbiguousProvisionOutcome(new Error("boom"))).toBe(false);
  });
});

/**
 * Codex review on #1828, PR comment 3878667845: `crypto.randomUUID` is a
 * secure-context-only API, so a console served over plain HTTP has `crypto`
 * but not `crypto.randomUUID`. The id-suffix generator's old fallback for
 * that case was `Math.random()` — a non-cryptographic PRNG — even though
 * `crypto.getRandomValues` (the one `Crypto` member usable from an insecure
 * context) was available the whole time. `resetReplacementId`'s own doc
 * comment says the suffix has to resist "a genuine collision with someone
 * else's company", not just an accidental one, which is exactly what a
 * predictable PRNG can't back.
 */
describe("resetReplacementId — insecure-context randomness fallback (issue #1828 comment 3878667845)", () => {
  afterEach(() => {
    vi.unstubAllGlobals();
  });

  it("prefers crypto.getRandomValues over Math.random when randomUUID is unavailable", () => {
    // Simulates a console served over plain HTTP: `crypto` exists (Node
    // supplies it globally), but `randomUUID` does not — the same shape a
    // browser gives it outside a secure context.
    const bytes = Uint8Array.from([0x0a, 0x1b, 0x2c, 0x3d]);
    const getRandomValues = vi.fn((arr: Uint8Array) => {
      arr.set(bytes);
      return arr;
    });
    vi.stubGlobal("crypto", { getRandomValues });

    const mathRandom = vi.spyOn(Math, "random");

    expect(resetReplacementId("acme")).toBe("acme-0a1b2c3d");
    expect(getRandomValues).toHaveBeenCalledTimes(1);
    expect(mathRandom).not.toHaveBeenCalled();

    mathRandom.mockRestore();
  });

  it("falls back to Math.random only when no Web Crypto API exists at all", () => {
    vi.stubGlobal("crypto", undefined);

    const mathRandom = vi.spyOn(Math, "random").mockReturnValue(0.123456789);

    const id = resetReplacementId("acme");
    expect(id.startsWith("acme-")).toBe(true);
    expect(mathRandom).toHaveBeenCalled();

    mathRandom.mockRestore();
  });
});
