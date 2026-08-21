// The company's custom mail domain and its own SMTP credentials, over the host
// (issue #1460).
//
// Reads come over GraphQL (`Company.domain` / `Company.smtp`), the host's one
// status-read plane for these; writes are REST (`PUT …/domain`,
// `POST …/domain/verify`, `PUT …/smtp`, `POST …/smtp/test`). Credentials are
// write-only: the SMTP password goes OUT with a save and never comes back — no
// read here returns it, and nothing stores it in the browser. The DNS records
// come from the host, not from the client: the console must not fabricate the
// records an operator publishes at their own registrar.

import type { OpenCompanyClient } from "@/api/client";

/** One DNS record the operator must publish. Mirrors the host's `DnsRecord`. */
export interface DnsRecord {
  type: string;
  name: string;
  value: string;
  ttl: string;
}

/** Custom-domain status. Mirrors the host's `DomainStatus`. */
export interface DomainStatus {
  domain: string;
  verified: boolean;
  records: DnsRecord[];
}

/**
 * Non-secret SMTP status as the GraphQL read projects it — host, port,
 * username, and whether anything is stored. Never the password, and (by the
 * host's projection) not the security mode or from-address either; those are
 * write-only inputs re-entered on a change, not read back.
 */
export interface SmtpStatus {
  host: string;
  port: number;
  username: string;
  configured: boolean;
}

export type SmtpSecurity = "none" | "starttls" | "ssl";

/**
 * The full SMTP credential a save writes. Wire keys are snake_case to match the
 * host's `SmtpCredentials`; `password` is write-only. A save REPLACES the stored
 * credential in full — the host has no partial-update route — so every field is
 * sent, not just the changed one.
 */
export interface SmtpCredentialsInput {
  host: string;
  port: number;
  security: SmtpSecurity;
  username: string;
  password: string;
  from_name: string;
  from_email: string;
}

/** The outcome of a test send. */
export interface SmtpTestResult {
  ok: boolean;
  message: string;
}

const MAIL_QUERY = `query CompanyMail($id: ID) {
  company(id: $id) {
    domain { domain verified records { type name value ttl } }
    smtp { host port username configured }
  }
}`;

interface MailQueryData {
  company: {
    domain: DomainStatus | null;
    smtp: SmtpStatus | null;
  } | null;
}

/**
 * The stored domain + SMTP status for a company, read over GraphQL.
 *
 * `domain` is `null` when none is set. `smtp` is always present — an
 * unconfigured company reads `configured: false` — so the card can tell "not
 * set" from "the host could not answer" (a thrown error) rather than conflating
 * them.
 */
export async function fetchMailStatus(
  client: OpenCompanyClient,
  company: string | null,
): Promise<{ domain: DomainStatus | null; smtp: SmtpStatus }> {
  const res = await client.graphqlRequest(MAIL_QUERY, { id: company });
  if (res.errors) {
    const message =
      Array.isArray(res.errors) && res.errors.length > 0
        ? String((res.errors[0] as { message?: unknown }).message ?? "GraphQL error")
        : "GraphQL error";
    throw new Error(message);
  }
  const data = res.data as MailQueryData | null | undefined;
  return {
    domain: data?.company?.domain ?? null,
    smtp: data?.company?.smtp ?? { host: "", port: 0, username: "", configured: false },
  };
}

/** Set (or clear, with an empty string) the custom domain; returns the host's records. */
export function putDomain(
  client: OpenCompanyClient,
  company: string | null,
  domain: string,
): Promise<DomainStatus> {
  return client.put<DomainStatus>(`${client.scopeFor(company)}/domain`, { domain });
}

/** Run a host-side DNS verification pass. 404 when the host has no DNS resolver. */
export function verifyDomain(
  client: OpenCompanyClient,
  company: string | null,
): Promise<DomainStatus> {
  return client.post<DomainStatus>(`${client.scopeFor(company)}/domain/verify`);
}

/** Store the company's SMTP credentials (write-only password); returns non-secret status. */
export function putSmtp(
  client: OpenCompanyClient,
  company: string | null,
  creds: SmtpCredentialsInput,
): Promise<SmtpStatus> {
  return client.put<SmtpStatus>(`${client.scopeFor(company)}/smtp`, creds);
}

/** Send a test email through the stored credentials. 404 when the host has no SMTP sender. */
export function testSmtp(
  client: OpenCompanyClient,
  company: string | null,
  to?: string,
): Promise<SmtpTestResult> {
  return client.post<SmtpTestResult>(
    `${client.scopeFor(company)}/smtp/test`,
    to ? { to } : {},
  );
}
