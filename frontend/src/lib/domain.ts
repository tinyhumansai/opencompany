// What is left of the browser-local domain/SMTP card (issue #1460).
//
// The card used to be a mock. It kept a whole `MailSettings` blob in
// `localStorage` — domain, SMTP host, username, from addresses — and fabricated
// DNS records client-side against a hardcoded target. The SMTP **password** was
// in that blob too, written back on every keystroke: readable by any script on
// the origin, surviving sign-out, with no expiry.
//
// The first half of #1460 stopped the password specifically. This half deleted
// the store. `Settings → General` now reads and writes the host
// (`src/api/domain.ts`, `src/api/smtp.ts`), so every field the draft used to
// remember has an authoritative answer and a remembered copy would only be a
// second one that disagrees.
//
// So the guarantee is now stronger than "the password is filtered out on the
// way in": **there is no writer left at all.** No function in this module, and
// no component, puts anything under an `oc-mail*` key. The password lives in
// React state for the life of the page, goes write-only to the host's secret
// store on Save, and is cleared out of the form when the save succeeds.
//
// Two things survive, for two different reasons:
//
//   - `isValidDomain` — a pre-flight so a typo gets a sentence instead of a
//     round-trip.
//   - `purgeStoredSmtpPasswords` — because stopping new writes does nothing for
//     the browsers that already have one. See its docstring.

/**
 * Whether a string looks like a hostname worth sending to the host.
 *
 * UX, not a guard. The host does not validate the domain, so nothing here is
 * protecting it; the point is that an operator who typed `acme` gets "Enter a
 * valid domain, e.g. mail.acme.com" immediately rather than a stored value that
 * can never verify.
 *
 * `""` deliberately fails — and callers rely on that only for the *add* path.
 * Removal is `PUT { domain: "" }`, which never comes through here: the Remove
 * button sends the empty sentinel directly rather than asking this function's
 * permission to.
 */
export function isValidDomain(domain: string): boolean {
  return /^(?!-)[a-z0-9-]+(\.[a-z0-9-]+)+$/i.test(domain.trim());
}

/** Every localStorage key this module has ever written a password under. */
const MAIL_KEY_PREFIX = "oc-mail";

/**
 * Removes SMTP passwords left in localStorage by the pre-#1460 console.
 *
 * Deleting the writer only half-solves it: an operator who typed a password
 * before upgrading still has it sitting in their browser, and it stays there
 * until something deletes it. This runs once at boot (see `main.tsx`) and
 * sweeps **every** `oc-mail*` key — scoped and legacy, every connection and
 * company, not just the scope the current page happens to be looking at,
 * because the operator is not required to visit Settings for the credential to
 * need to be gone.
 *
 * It rewrites rather than deletes: host, port, security, username and the from
 * fields are not secret and are work the operator did, so a browser that still
 * has them keeps them readable. Only the password is dropped. A key whose JSON
 * cannot be parsed, or which still mentions a password after the strip, is
 * removed outright — it cannot be shown to be password-free, and this
 * function's contract is that afterwards no password remains.
 *
 * Returns the number of keys it rewrote or removed, which is what the
 * regression test asserts on.
 */
export function purgeStoredSmtpPasswords(): number {
  let cleaned = 0;
  try {
    const store = window.localStorage;
    const keys: string[] = [];
    for (let i = 0; i < store.length; i++) {
      const key = store.key(i);
      if (key !== null && key.startsWith(MAIL_KEY_PREFIX)) keys.push(key);
    }
    for (const key of keys) {
      const raw = store.getItem(key);
      if (raw === null) continue;
      if (!raw.includes('"password"')) continue;
      let parsed: unknown;
      try {
        parsed = JSON.parse(raw);
      } catch {
        // Unreadable, but it contains the word. Cannot be proven clean, so it
        // goes — a stale draft is a far smaller loss than a retained secret.
        store.removeItem(key);
        cleaned++;
        continue;
      }
      const rewritten = JSON.stringify(stripPasswords(parsed));
      // Belt and braces: the strip is structural, so this should never fire.
      // If it does, the blob has a shape this function does not understand and
      // the only safe reading of "no password remains" is to remove it.
      if (rewritten.includes('"password"')) store.removeItem(key);
      else store.setItem(key, rewritten);
      cleaned++;
    }
  } catch {
    /* storage unavailable — nothing was ever stored to clean */
  }
  return cleaned;
}

/**
 * Returns `value` with every `password` key removed, at any depth.
 *
 * Recursive rather than reaching for `blob.smtp.password`, because the old
 * shape is not the only shape this can meet: keys written by intermediate
 * builds, and hand-edited blobs, both exist in the wild. The contract is about
 * the whole value, so the strip has to be about the whole value too.
 */
function stripPasswords(value: unknown): unknown {
  if (Array.isArray(value)) return value.map(stripPasswords);
  if (value !== null && typeof value === "object") {
    const out: Record<string, unknown> = {};
    for (const [k, v] of Object.entries(value as Record<string, unknown>)) {
      if (k === "password") continue;
      out[k] = stripPasswords(v);
    }
    return out;
  }
  return value;
}
