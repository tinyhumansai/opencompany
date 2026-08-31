// Reaching the browser wallet that signs a sign-in challenge.
//
// Deliberately a thin adapter over the provider a Solana wallet injects, rather
// than a wallet-adapter dependency. Two reasons:
//
//  1. The console needs exactly two operations — "what is your address" and
//     "sign these bytes" — and every injected provider (Phantom, Solflare,
//     Backpack) exposes both under the same names. A framework would add a
//     dependency, a React context, and a modal for a two-call surface.
//  2. Nothing here decides anything. The host issues the challenge and verifies
//     the signature; this file only carries bytes between the wallet and the
//     API. A bug here cannot admit anyone — the worst it does is fail to sign.

/** The subset of an injected Solana provider this console uses. */
interface InjectedWallet {
  isConnected?: boolean;
  connect(): Promise<{ publicKey: { toString(): string } }>;
  signMessage(message: Uint8Array, encoding?: string): Promise<{ signature: Uint8Array }>;
}

declare global {
  interface Window {
    solana?: InjectedWallet;
    phantom?: { solana?: InjectedWallet };
  }
}

/** The injected provider, if this browser has one. */
function provider(): InjectedWallet | undefined {
  // Phantom namespaces itself as well as taking `window.solana`, and a browser
  // with two wallets installed may have only one of the two set.
  return window.phantom?.solana ?? window.solana;
}

/** Whether a wallet is available to sign at all. */
export function hasWallet(): boolean {
  return provider() !== undefined;
}

/** Raised when there is nothing installed to sign with. */
export class NoWalletError extends Error {
  constructor() {
    super("No wallet extension found in this browser.");
    this.name = "NoWalletError";
  }
}

/** Connects, and returns the base58 address the wallet holds. */
export async function connectWallet(): Promise<string> {
  const wallet = provider();
  if (!wallet) throw new NoWalletError();
  const { publicKey } = await wallet.connect();
  return publicKey.toString();
}

/**
 * Signs `message` and returns the signature, base58-encoded.
 *
 * The host verifies base58 because that is the encoding a Solana address and
 * signature already travel in; encoding here rather than sending raw bytes
 * keeps the request body plain JSON.
 */
export async function signMessage(message: string): Promise<string> {
  const wallet = provider();
  if (!wallet) throw new NoWalletError();
  const bytes = new TextEncoder().encode(message);
  const { signature } = await wallet.signMessage(bytes, "utf8");
  return base58(signature);
}

/** The base58 alphabet Bitcoin and Solana use. */
const ALPHABET = "123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";

/**
 * Base58-encodes bytes.
 *
 * Written out rather than pulled in, because it is fifteen lines and the
 * alternative is a dependency on the critical path of signing in. Leading zero
 * bytes encode as leading `1`s, which is what makes the encoding
 * length-preserving for keys that start with a zero byte.
 */
export function base58(bytes: Uint8Array): string {
  // `digits` starts empty rather than seeded with a sentinel `0`: a sentinel
  // that is never overwritten (every byte is zero, including the empty-input
  // case) would still emit one extra digit below, double-counting the leading
  // zero the loop after this one already writes.
  const digits: number[] = [];
  for (const byte of bytes) {
    let carry = byte;
    for (let i = 0; i < digits.length; i += 1) {
      carry += digits[i] << 8;
      digits[i] = carry % 58;
      carry = (carry / 58) | 0;
    }
    while (carry > 0) {
      digits.push(carry % 58);
      carry = (carry / 58) | 0;
    }
  }
  let out = "";
  for (const byte of bytes) {
    if (byte !== 0) break;
    out += ALPHABET[0];
  }
  for (let i = digits.length - 1; i >= 0; i -= 1) out += ALPHABET[digits[i]];
  return out;
}

/**
 * Decodes a base58 string back into the bytes it encodes, or `null` when it
 * contains a character outside the alphabet.
 *
 * The mirror of [`base58`], written out for the same reason — and the one
 * consumer, deciding whether a `wallet:`-prefixed identity key really is a
 * wallet key, must agree with the host's `decode_wallet_address`
 * (`src/ports/users.rs`), which is the same check on the same bytes. Leading
 * `1`s decode back to the leading zero bytes the encoder promised to preserve.
 */
export function base58ToBytes(value: string): Uint8Array | null {
  // `digits` is the value in base 256, least-significant byte first.
  const digits: number[] = [];
  for (const ch of value) {
    const digit = ALPHABET.indexOf(ch);
    if (digit === -1) return null;
    let carry = digit;
    for (let i = 0; i < digits.length; i += 1) {
      carry += digits[i] * 58;
      digits[i] = carry & 0xff;
      carry >>= 8;
    }
    while (carry > 0) {
      digits.push(carry & 0xff);
      carry >>= 8;
    }
  }
  // Leading `1`s in the base58 form are leading zero bytes.
  let zeros = 0;
  for (const ch of value) {
    if (ch !== ALPHABET[0]) break;
    zeros += 1;
  }
  const out = new Uint8Array(zeros + digits.length);
  for (let i = 0; i < digits.length; i += 1) out[zeros + i] = digits[digits.length - 1 - i];
  return out;
}
