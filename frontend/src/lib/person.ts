// What to call a person, and which face to draw for them.
//
// A person who has not named themselves still has to be called something on
// every surface that shows them, and the honest options are the raw address,
// nothing, or a guess. The raw address is refused on the host's own rule — being
// in a company should not hand everyone your mailbox — and nothing leaves a chat
// message attributed to a blank. So: a guess, made here, at render time.
//
// The rule mirrors `derive_display_name` in `src/ports/users.rs`, which is what
// the host itself uses when it has to name somebody (in invite mail, for
// instance). Two copies of one rule is the price of the console not needing a
// round trip to render a name; they are kept in step by both being small and by
// both being tested.

import { avatarRef } from "@/lib/avatar";
import { base58ToBytes } from "@/lib/wallet";

/** The shape both `Me` and `Person` satisfy — the two ways a person arrives. */
export interface NamedPerson {
  id: string;
  /** The login identity key: an address in email mode, `wallet:…` / `local:owner` otherwise. */
  email: string;
  /** What they chose to be called, absent when they have not chosen. */
  displayName?: string;
  /** The face they chose, absent when they have not chosen. */
  avatar?: string;
}

/**
 * A readable name guessed from a login identity — `steven.enamakel@acme.com`
 * reads as "Steven Enamakel".
 *
 * Only the **local part**, split on the separators people actually use, with
 * each word capitalised. The domain is dropped: it identifies the mailbox rather
 * than the person.
 *
 * `null` for an identity with no name in it to find — a wallet key, the local
 * owner of a company with no sign-in, or a local part with no letters. `null`
 * means "cannot say", and a caller renders something honest rather than a guess
 * this function refused to make.
 */
export function guessName(identityKey: string): string | null {
  // A wallet key and the implicit local owner are identities, not names.
  // Capitalising either would produce something that *looks* like a name.
  //
  // The prefixes are judged the way the host parses them (`LoginIdentity::parse`
  // in `src/ports/users.rs`), not by prefix alone: an address like
  // `wallet:ada@example.com` is an *email* whose local part merely starts with
  // the prefix, and so is `local:owner@example.com` — only a base58 string that
  // actually decodes to a 32-byte Ed25519 key after `wallet:`, and the exact
  // value `local:owner`, are identities with no name in them. Guessing less
  // strictly than the host would render the same person differently on the two
  // sides.
  if (identityKey.startsWith("wallet:")) {
    const address = identityKey.slice("wallet:".length).trim();
    const bytes = base58ToBytes(address);
    if (bytes && bytes.length === 32) return null;
  } else if (identityKey === "local:owner") {
    return null;
  }
  const local = identityKey.split("@")[0].split("+")[0];
  const words = local
    .split(/[._-]+/)
    .filter(Boolean)
    // Only the first character is touched: lower-casing the rest would turn
    // `McDonald` into `Mcdonald`, and a local part carrying capitals is one
    // somebody chose to write that way.
    .map((word) => word.charAt(0).toUpperCase() + word.slice(1));
  if (words.length === 0 || !words.some((w) => /\p{L}/u.test(w))) return null;
  return words.join(" ");
}

/**
 * What to call this person on screen: the name they chose, else a guess, else
 * the identity key itself.
 *
 * The last fallback is deliberate and is the only place a raw key is rendered:
 * a wallet-signed-in person has no name to guess, and showing their key beats
 * showing a blank where a name belongs. Callers with somewhere better to go — a
 * shortened key, a role noun — should use {@link guessName} directly and decide
 * for themselves.
 */
export function personName(person: NamedPerson): string {
  return person.displayName?.trim() || guessName(person.email) || person.email;
}

/** The face to draw for a person: what they chose, else the mascot hashed from their id. */
export function personAvatar(person: NamedPerson): string {
  return avatarRef(person.avatar, person.id || person.email);
}
