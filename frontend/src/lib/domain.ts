// Custom domain + SMTP setup for a company. Persisted per company in
// localStorage; the console has no provisioning API yet, so DNS verification
// and SMTP tests are host-side once wired. Secrets live here only as a local
// draft until the host stores them securely.

export interface DomainConfig {
  domain: string;
  verified: boolean;
}

export type SmtpSecurity = "none" | "starttls" | "ssl";

export interface SmtpConfig {
  host: string;
  port: string;
  security: SmtpSecurity;
  username: string;
  password: string;
  fromName: string;
  fromEmail: string;
}

export interface MailSettings {
  domain: DomainConfig;
  smtp: SmtpConfig;
}

export function emptyMailSettings(): MailSettings {
  return {
    domain: { domain: "", verified: false },
    smtp: { host: "", port: "587", security: "starttls", username: "", password: "", fromName: "", fromEmail: "" },
  };
}

/** The platform host records point at. In production this comes from the host. */
const PLATFORM_TARGET = "mail.opencompany.host";

export interface DnsRecord {
  type: "CNAME" | "TXT";
  name: string;
  value: string;
  ttl: string;
}

/** A short, stable verification token derived from the domain. */
function verifyToken(domain: string): string {
  let hash = 0;
  for (let i = 0; i < domain.length; i++) hash = (hash * 31 + domain.charCodeAt(i)) | 0;
  return Math.abs(hash).toString(16).padStart(8, "0");
}

/** The DNS records a user must add to point a custom domain at the platform
 *  and let it send email (verification + CNAME + DKIM + SPF). */
export function dnsRecords(domain: string): DnsRecord[] {
  const d = domain.trim().replace(/\.$/, "");
  if (!d) return [];
  return [
    { type: "TXT", name: `_opencompany.${d}`, value: `oc-verify=${verifyToken(d)}`, ttl: "3600" },
    { type: "CNAME", name: d, value: PLATFORM_TARGET, ttl: "3600" },
    { type: "CNAME", name: `oc1._domainkey.${d}`, value: `oc1.dkim.opencompany.host`, ttl: "3600" },
    { type: "CNAME", name: `oc2._domainkey.${d}`, value: `oc2.dkim.opencompany.host`, ttl: "3600" },
    { type: "TXT", name: d, value: "v=spf1 include:spf.opencompany.host ~all", ttl: "3600" },
  ];
}

export function isValidDomain(domain: string): boolean {
  return /^(?!-)[a-z0-9-]+(\.[a-z0-9-]+)+$/i.test(domain.trim());
}

const KEY = (company: string | null) => `oc-mail:${company ?? "single"}`;

export function loadMailSettings(company: string | null): MailSettings {
  try {
    const raw = localStorage.getItem(KEY(company));
    if (raw) return { ...emptyMailSettings(), ...(JSON.parse(raw) as MailSettings) };
  } catch {
    /* fall through */
  }
  return emptyMailSettings();
}

export function saveMailSettings(company: string | null, settings: MailSettings): void {
  try {
    localStorage.setItem(KEY(company), JSON.stringify(settings));
  } catch {
    /* storage unavailable */
  }
}
