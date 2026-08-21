// Domain validation for the custom-mail settings card.
//
// The DNS records, the verification token and the SMTP credentials all live on
// the host now (issue #1460): the console reads status over GraphQL and writes
// over `…/domain` and `…/smtp`, so nothing here fabricates records or persists a
// secret. What is left is the one purely-local question worth answering before a
// round trip — whether a string is even shaped like a domain.

/** Whether `domain` is shaped like a hostname (at least one dot, valid labels). */
export function isValidDomain(domain: string): boolean {
  return /^(?!-)[a-z0-9-]+(\.[a-z0-9-]+)+$/i.test(domain.trim());
}

/**
 * Parses an SMTP port string to a valid TCP port, or `null` when it is not one.
 *
 * The host's `port` is a `u16`, so a non-integer, a zero, or anything above
 * 65535 must be caught before the round trip rather than surfacing as an opaque
 * deserialize failure.
 */
export function parseSmtpPort(value: string): number | null {
  const trimmed = value.trim();
  if (!/^\d+$/.test(trimmed)) return null;
  const port = Number(trimmed);
  return port >= 1 && port <= 65535 ? port : null;
}
