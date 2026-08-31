import { describe, expect, it } from "vitest";

import { base58, base58ToBytes } from "@/lib/wallet";

/**
 * `base58ToBytes` is the decoder half of `base58`, and its one real consumer —
 * `guessName` in `@/lib/person` deciding whether a `wallet:`-prefixed key is a
 * wallet — must agree with the host's `decode_wallet_address`
 * (`src/ports/users.rs`). The two properties that matter there are that the
 * decode round-trips through the encoder, and that a character outside the
 * alphabet fails the whole string rather than silently skipping it.
 */
describe("base58ToBytes", () => {
  it("round-trips through base58", () => {
    const bytes = new Uint8Array([7, 8, 9, 250, 251]);
    expect(base58ToBytes(base58(bytes))).toEqual(bytes);
    const empty = new Uint8Array([]);
    expect(base58ToBytes(base58(empty))).toEqual(empty);
  });

  it("preserves leading zero bytes as leading 1s", () => {
    expect(base58ToBytes("11")).toEqual(new Uint8Array([0, 0]));
    expect(base58ToBytes("1z")).toEqual(new Uint8Array([0, 57]));
  });

  it("rejects a character outside the alphabet rather than skipping it", () => {
    expect(base58ToBytes("not base58!")).toBeNull();
    expect(base58ToBytes("7cVfg@ArChe")).toBeNull();
  });
});
