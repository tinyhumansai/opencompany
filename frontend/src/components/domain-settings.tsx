import { useCallback, useEffect, useState } from "react";
import { Check, Copy, Globe, Loader2, Mail, ShieldAlert, TriangleAlert, X } from "lucide-react";
import { toast } from "sonner";

import { ApiError } from "@/api/types";
import { AdminOnlyNotice } from "@/components/admin-only-notice";
import type { OpenCompanyClient } from "@/api/client";
import {
  clearDomain,
  type DnsRecord,
  type DomainStatus,
  getDomain,
  type RecordCheck,
  saveDomain,
  verifyDomain,
} from "@/api/domain";
import { getSmtp, saveSmtp, type SmtpSecurity, type SmtpStatus, testSmtp } from "@/api/smtp";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { isValidDomain } from "@/lib/domain";
import { cn } from "@/lib/utils";

interface Props {
  client: OpenCompanyClient;
  company: string | null;
  /**
   * Whether this viewer may change the company's mail identity.
   *
   * `PUT …/domain` and the SMTP writes are `AdminScopedCompany`. The reads are
   * not, and neither is `POST …/domain/verify` — re-checking DNS for a domain
   * only an admin could have set changes nothing a member could not already
   * read — so a member keeps the whole card except the controls that write.
   */
  canManage: boolean;
}

const SECURITY_LABELS: Record<SmtpSecurity, string> = {
  none: "None",
  starttls: "STARTTLS",
  ssl: "SSL / TLS",
};

/** The message out of a rejected request, whatever it was rejected with. */
function reason(err: unknown): string {
  return err instanceof Error ? err.message : String(err);
}

/**
 * Whether a rejection means "this build has no such feature".
 *
 * The `code`, never the status. Both of these routes can also answer a plain
 * 404 or a 400 for ordinary reasons — verify refuses with 400 when no domain is
 * configured — and a status check would read a real refusal as a missing
 * feature and hide the operator's actual problem behind a build notice.
 */
function isUnwired(err: unknown): boolean {
  return err instanceof ApiError && err.code === "not_wired";
}

/**
 * Settings → General: the company's custom domain and its outbound mail server.
 *
 * # This was a mock until #1460
 *
 * Both cards were browser-local. The domain card hashed the domain into a fake
 * verification token, pasted it into a hardcoded `opencompany.host` target, and
 * rendered five DNS records the host had never heard of; its "Pending" badge
 * pulsed forever because nothing checked anything. "Verify DNS" and "Test
 * connection" each fired a `toast.info` saying the real thing would happen once
 * connected. The host had implemented all of it and the console had never
 * called it.
 *
 * Now each card owns one route pair and renders only what the host reported.
 * They are deliberately independent components rather than two halves of one
 * `MailSettings` object: different routes, different authority, different
 * failure modes. A domain read that fails must not blank the SMTP form.
 *
 * # The password
 *
 * Write-only three ways, exactly as in `HostingView`: the host has no field to
 * return it, nothing persists it to browser storage, and a successful save
 * clears the input. See `src/api/smtp.ts` and `src/lib/domain.ts`.
 */
export function DomainSettings({ client, company, canManage }: Props) {
  return (
    <>
      <DomainCard client={client} company={company} canManage={canManage} />
      <SmtpCard client={client} company={company} canManage={canManage} />
    </>
  );
}

function DomainCard({ client, company, canManage }: Props) {
  const [status, setStatus] = useState<DomainStatus | null>(null);
  const [loading, setLoading] = useState(true);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [draft, setDraft] = useState("");
  // A build fact, so it is sticky for the life of the card rather than a toast:
  // the operator is looking at the Verify button when they learn this, and a
  // notice that vanishes leaves them clicking a button that cannot work.
  const [verifyUnwired, setVerifyUnwired] = useState(false);

  const load = useCallback(async () => {
    setLoading(true);
    try {
      const next = await getDomain(client, company);
      setStatus(next);
      setLoadError(null);
    } catch (err) {
      setLoadError(reason(err));
    } finally {
      setLoading(false);
    }
  }, [client, company]);

  useEffect(() => {
    void load();
  }, [load]);

  const configured = Boolean(status?.domain);

  async function connect() {
    const domain = draft.trim().toLowerCase();
    // Pre-flight only — the host does not validate, so this exists to turn a
    // typo into a sentence instead of a stored value that can never verify.
    if (!isValidDomain(domain)) {
      toast.error("Enter a valid domain, e.g. mail.acme.com");
      return;
    }
    setBusy(true);
    try {
      setStatus(await saveDomain(client, company, domain));
      toast.success("Domain saved — add the DNS records below.");
    } catch (err) {
      toast.error(reason(err));
    } finally {
      setBusy(false);
    }
  }

  async function remove() {
    setBusy(true);
    try {
      setStatus(await clearDomain(client, company));
      setDraft("");
      // A fresh domain deserves a fresh verdict on whether verification works.
      setVerifyUnwired(false);
      toast.success("Domain removed.");
    } catch (err) {
      toast.error(reason(err));
    } finally {
      setBusy(false);
    }
  }

  async function verify() {
    // The host answers 400 when nothing is configured; never ask it.
    if (!configured) return;
    setBusy(true);
    try {
      const next = await verifyDomain(client, company);
      setStatus(next);
      if (next.verified) toast.success("Domain verified.");
      else toast.message("Records not found yet — DNS can take up to 48h to propagate.");
    } catch (err) {
      // No success toast and no badge change on this path: the card must never
      // render a state the host did not report.
      if (isUnwired(err)) setVerifyUnwired(true);
      else toast.error(reason(err));
    } finally {
      setBusy(false);
    }
  }

  return (
    <Card data-testid="domain-card">
      <CardHeader>
        <CardTitle className="flex items-center gap-2 text-base">
          <Globe className="size-4" /> Custom domain
        </CardTitle>
        <CardDescription>Send and receive on your own domain instead of the default.</CardDescription>
      </CardHeader>
      <CardContent className="space-y-4">
        {!canManage && (
          <AdminOnlyNotice
            testId="domain-read-only"
            title="Only an admin can change this company's domain"
          >
            The domain is how this company signs its outgoing mail, so it is the
            company&rsquo;s identity rather than any one member&rsquo;s. You can see
            what is configured and re-check the DNS records.
          </AdminOnlyNotice>
        )}
        {loadError ? (
          <Alert variant="destructive" data-testid="domain-load-error">
            <TriangleAlert className="size-4" />
            <AlertDescription>Could not load the domain settings: {loadError}</AlertDescription>
          </Alert>
        ) : loading ? (
          <p className="flex items-center gap-2 text-sm text-muted-foreground">
            <Loader2 className="size-4 animate-spin" /> Loading domain…
          </p>
        ) : !configured ? (
          !canManage ? (
            <p className="text-sm text-muted-foreground">
              No custom domain is configured, so this company sends on the default one.
            </p>
          ) : (
          <div className="flex flex-col gap-2 sm:flex-row">
            <Input
              value={draft}
              data-testid="domain-input"
              // The card title is the only thing naming this field, and a
              // screen reader does not read it as the input's name. The
              // placeholder is an example, not a label — it disappears on the
              // first keystroke, which is exactly when it would be needed.
              aria-label="Custom domain"
              onChange={(e) => setDraft(e.target.value)}
              placeholder="mail.acme.com"
              onKeyDown={(e) => e.key === "Enter" && void connect()}
            />
            <Button
              className="shrink-0"
              disabled={busy}
              onClick={() => void connect()}
              data-testid="domain-add"
            >
              Add domain
            </Button>
          </div>
          )
        ) : (
          <>
            <div className="flex flex-wrap items-center justify-between gap-2 rounded-lg border p-3">
              <span className="inline-flex items-center gap-2 font-mono text-sm">
                <Globe className="size-4 text-muted-foreground" />
                {status?.domain}
              </span>
              <div className="flex items-center gap-2">
                {status?.verified ? (
                  <Badge
                    className="gap-1 bg-status-done-soft text-status-done-text"
                    data-testid="domain-verified"
                  >
                    <Check className="size-3" /> Verified
                  </Badge>
                ) : (
                  <Badge variant="secondary" className="gap-1" data-testid="domain-pending">
                    <span className="size-1.5 rounded-full bg-status-blocked" /> Pending
                  </Badge>
                )}
                {canManage && (
                  <Button
                    variant="ghost"
                    size="sm"
                    disabled={busy}
                    onClick={() => void remove()}
                    data-testid="domain-remove"
                  >
                    Remove
                  </Button>
                )}
              </div>
            </div>

            {!status?.verified ? (
              <p className="text-xs text-muted-foreground" data-testid="domain-check-summary">
                {checkSummary(status?.checks, status?.records ?? [])}
              </p>
            ) : null}

            <div className="space-y-2">
              <p className="text-sm font-medium">Add these DNS records</p>
              {/* Straight off the status. Never re-derived here — see the
                  module header of `src/api/domain.ts`. */}
              <DnsTable records={status?.records ?? []} checks={status?.checks} />

              {verifyUnwired ? (
                <Alert data-testid="domain-verify-unwired">
                  <TriangleAlert className="size-4" />
                  <AlertDescription>
                    This host was built without DNS lookups, so it can&rsquo;t check these
                    records for you. Add them at your registrar; a build with the{" "}
                    <code>dns</code> feature will verify them.
                  </AlertDescription>
                </Alert>
              ) : null}

              <div className="flex items-center gap-2 pt-1">
                <Button
                  variant="outline"
                  size="sm"
                  disabled={busy || verifyUnwired}
                  onClick={() => void verify()}
                  data-testid="domain-verify"
                >
                  {busy ? <Loader2 className="mr-2 size-4 animate-spin" /> : null}
                  Verify DNS
                </Button>
                <p className="text-xs text-muted-foreground">
                  Changes can take up to 48h to propagate.
                </p>
              </div>
            </div>
          </>
        )}
      </CardContent>
    </Card>
  );
}

/**
 * What the subtext under a Pending badge says.
 *
 * "Not checked yet" and "0 of 5 records found" mean different things to the
 * operator — the first is on them to press the button, the second is on their
 * registrar or on propagation — and telling them apart is the entire reason the
 * host returns `checks` at all rather than just `verified`.
 */
function checkSummary(checks: RecordCheck[] | undefined, records: DnsRecord[]): string {
  if (checks === undefined) return "Not checked yet.";
  const found = checks.filter((c) => c.found).length;
  const total = records.length || checks.length;
  return `${found} of ${total} records found.`;
}

function DnsTable({ records, checks }: { records: DnsRecord[]; checks?: RecordCheck[] }) {
  return (
    <div className="overflow-x-auto rounded-lg border">
      <table className="w-full text-left text-xs">
        <thead className="bg-muted/50 text-muted-foreground">
          <tr>
            <th className="px-3 py-2 font-medium">Type</th>
            <th className="px-3 py-2 font-medium">Name</th>
            <th className="px-3 py-2 font-medium">Value</th>
            <th className="px-3 py-2 font-medium">TTL</th>
            {checks ? <th className="px-3 py-2 font-medium">Found</th> : null}
          </tr>
        </thead>
        <tbody className="divide-y">
          {records.map((r) => {
            // Matched by (name, type), never by index. The host is free to
            // return checks in another order or to check a subset, and an
            // index-matched join would put a tick on the wrong row without
            // anything on screen looking wrong.
            const check = checks?.find((c) => c.name === r.name && c.type === r.type);
            return (
              <tr key={`${r.type}:${r.name}`} className="align-top" data-testid="dns-record-row">
                <td className="px-3 py-2">
                  <Badge variant="outline" className="font-mono">
                    {r.type}
                  </Badge>
                </td>
                <td className="px-3 py-2">
                  <CopyCell value={r.name} />
                </td>
                <td className="px-3 py-2">
                  <CopyCell value={r.value} />
                </td>
                <td className="px-3 py-2 font-mono text-muted-foreground">{r.ttl}</td>
                {checks ? (
                  <td className="px-3 py-2" data-testid="dns-record-check">
                    {check === undefined ? (
                      <span className="text-muted-foreground">—</span>
                    ) : check.found ? (
                      <Check className="size-3.5 text-status-done-text" aria-label="found" />
                    ) : (
                      <X className="size-3.5 text-status-blocked" aria-label="not found" />
                    )}
                  </td>
                ) : null}
              </tr>
            );
          })}
        </tbody>
      </table>
    </div>
  );
}

function CopyCell({ value }: { value: string }) {
  const [copied, setCopied] = useState(false);
  function copy() {
    void navigator.clipboard?.writeText(value);
    setCopied(true);
    setTimeout(() => setCopied(false), 1200);
  }
  return (
    <button
      onClick={copy}
      className="group flex max-w-[28ch] items-center gap-1.5 text-left sm:max-w-[40ch]"
      title="Copy"
    >
      <span className="truncate font-mono">{value}</span>
      {copied ? (
        <Check className="size-3 shrink-0 text-status-done-text" />
      ) : (
        <Copy className="size-3 shrink-0 text-muted-foreground opacity-0 transition-opacity group-hover:opacity-100" />
      )}
    </button>
  );
}

function SmtpCard({ client, company, canManage }: Props) {
  const [status, setStatus] = useState<SmtpStatus | null>(null);
  const [loading, setLoading] = useState(true);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [testUnwired, setTestUnwired] = useState(false);

  const [host, setHost] = useState("");
  const [port, setPort] = useState("587");
  const [security, setSecurity] = useState<SmtpSecurity>("starttls");
  const [username, setUsername] = useState("");
  const [fromName, setFromName] = useState("");
  const [fromEmail, setFromEmail] = useState("");
  // Always seeded empty and never prefilled with dots: the host has no field to
  // return it from, and an input full of placeholder characters invites an
  // operator to "correct" a value they cannot see — submitting the dots would
  // store the dots.
  const [password, setPassword] = useState("");

  const load = useCallback(async () => {
    setLoading(true);
    try {
      const next = await getSmtp(client, company);
      setStatus(next);
      setLoadError(null);
      setHost(next.host ?? "");
      setPort(next.port === undefined ? "587" : String(next.port));
      setSecurity(next.security ?? "starttls");
      setUsername(next.username ?? "");
      setFromName(next.from_name ?? "");
      setFromEmail(next.from_email ?? "");
      setPassword("");
    } catch (err) {
      setLoadError(reason(err));
    } finally {
      setLoading(false);
    }
  }, [client, company]);

  useEffect(() => {
    void load();
  }, [load]);

  async function save() {
    // The host takes a `u16`, so anything outside this range comes back as a
    // serde deserialize error an operator cannot act on. Caught here instead.
    const portNumber = Number(port.trim());
    if (!Number.isInteger(portNumber) || portNumber < 1 || portNumber > 65535) {
      toast.error("Port must be a whole number between 1 and 65535.");
      return;
    }
    setBusy(true);
    try {
      const next = await saveSmtp(client, company, {
        host: host.trim(),
        port: portNumber,
        // Always sent explicitly: it has no safe default on the host, and a
        // silently-omitted `security` is the difference between STARTTLS and
        // plaintext on the wire.
        security,
        username: username.trim(),
        from_email: fromEmail.trim(),
        from_name: fromName.trim(),
        // Omitted when blank, which is how "leave the stored one alone" is
        // expressed. Sending "" would clear it.
        ...(password ? { password } : {}),
      });
      setStatus(next);
      // Same reason `HostingView` clears its API key: a credential left sitting
      // in a form field is one screen-share from a leak.
      setPassword("");
      toast.success("Email settings saved.");
    } catch (err) {
      toast.error(reason(err));
    } finally {
      setBusy(false);
    }
  }

  async function test() {
    setBusy(true);
    try {
      const res = await testSmtp(client, company);
      // The host's own sentence, verbatim, on both branches — it knows whether
      // the server refused the credentials, timed out, or rejected the From
      // address, and a generic replacement throws that away.
      if (res.ok) toast.success(res.message);
      else toast.error(res.message);
    } catch (err) {
      if (isUnwired(err)) setTestUnwired(true);
      else toast.error(reason(err));
    } finally {
      setBusy(false);
    }
  }

  if (loadError) {
    return (
      <Card data-testid="smtp-card">
        <CardHeader>
          <CardTitle className="flex items-center gap-2 text-base">
            <Mail className="size-4" /> Email (SMTP)
          </CardTitle>
        </CardHeader>
        <CardContent>
          <Alert variant="destructive" data-testid="smtp-load-error">
            <TriangleAlert className="size-4" />
            <AlertDescription>Could not load the email settings: {loadError}</AlertDescription>
          </Alert>
        </CardContent>
      </Card>
    );
  }

  // Whether the host has a *stored* configuration to test — not whether the
  // form on screen looks filled in. The test send goes through what the host
  // holds, so gating on the form would enable the button on a typed-but-unsaved
  // password and report a verdict about something else entirely. That is the
  // same class of lie the card had before #1460, one button along.
  const testable = Boolean(status?.configured);

  return (
    <Card data-testid="smtp-card">
      <CardHeader>
        <CardTitle className="flex items-center gap-2 text-base">
          <Mail className="size-4" /> Email (SMTP)
        </CardTitle>
        <CardDescription>The outbound mail server your company sends through.</CardDescription>
      </CardHeader>
      <CardContent className="space-y-4">
        {loading ? (
          <p className="flex items-center gap-2 text-sm text-muted-foreground">
            <Loader2 className="size-4 animate-spin" /> Loading email settings…
          </p>
        ) : !canManage ? (
          // The mutations are withheld (`save`, `test`, and the password
          // field), not the routing: `GET …/smtp` is member-readable and
          // never carries a password by construction (`docs/modules/server/
          // authority.md`), so a member keeps the same read the admin form
          // shows them, just not editable.
          <>
            <AdminOnlyNotice
              testId="smtp-read-only"
              title="Only an admin can change how this company sends mail"
            >
              These are the credentials for the company&rsquo;s own outbound mail
              server, so an admin holds them.
            </AdminOnlyNotice>
            {status?.configured ? (
              <div className="grid gap-4 sm:grid-cols-2" data-testid="smtp-routing">
                <ReadOnlyField label="SMTP host" id="smtp-host" value={status.host} />
                <div className="grid grid-cols-2 gap-3">
                  <ReadOnlyField
                    label="Port"
                    id="smtp-port"
                    value={status.port === undefined ? undefined : String(status.port)}
                  />
                  <ReadOnlyField
                    label="Security"
                    id="smtp-security"
                    value={status.security ? SECURITY_LABELS[status.security] : undefined}
                  />
                </div>
                <ReadOnlyField label="Username" id="smtp-username" value={status.username} />
                <ReadOnlyField label="From name" id="smtp-from-name" value={status.from_name} />
                <ReadOnlyField label="From email" id="smtp-from-email" value={status.from_email} />
              </div>
            ) : (
              <p className="text-sm text-muted-foreground" data-testid="smtp-member-summary">
                No outbound mail server is configured, so this company sends on the host's default.
              </p>
            )}
          </>
        ) : (
          <>
            <div className="grid gap-4 sm:grid-cols-2">
              <Field label="SMTP host" id="smtp-host">
                <Input
                  id="smtp-host"
                  data-testid="smtp-host"
                  value={host}
                  onChange={(e) => setHost(e.target.value)}
                  placeholder="smtp.postmarkapp.com"
                />
              </Field>
              <div className="grid grid-cols-2 gap-3">
                <Field label="Port" id="smtp-port">
                  <Input
                    id="smtp-port"
                    data-testid="smtp-port"
                    value={port}
                    onChange={(e) => setPort(e.target.value)}
                    placeholder="587"
                    inputMode="numeric"
                  />
                </Field>
                <Field label="Security" id="smtp-security">
                  <Select
                    value={security}
                    onValueChange={(v) => v && setSecurity(v as SmtpSecurity)}
                    items={SECURITY_LABELS}
                  >
                    <SelectTrigger id="smtp-security" className="w-full">
                      <SelectValue />
                    </SelectTrigger>
                    <SelectContent>
                      {(Object.keys(SECURITY_LABELS) as SmtpSecurity[]).map((k) => (
                        <SelectItem key={k} value={k}>
                          {SECURITY_LABELS[k]}
                        </SelectItem>
                      ))}
                    </SelectContent>
                  </Select>
                </Field>
              </div>
              <Field label="Username" id="smtp-user">
                <Input
                  id="smtp-user"
                  data-testid="smtp-username"
                  value={username}
                  onChange={(e) => setUsername(e.target.value)}
                  placeholder="apikey"
                  autoComplete="off"
                />
              </Field>
              <Field
                label={status?.configured ? "Password (stored — leave blank to keep)" : "Password"}
                id="smtp-pass"
                hint="Sent to the host's secret store on Save, and never returned. Never written to browser storage."
              >
                <Input
                  id="smtp-pass"
                  data-testid="smtp-password"
                  type="password"
                  value={password}
                  onChange={(e) => setPassword(e.target.value)}
                  autoComplete="off"
                />
              </Field>
              <Field label="From name" id="smtp-fromname">
                <Input
                  id="smtp-fromname"
                  data-testid="smtp-from-name"
                  value={fromName}
                  onChange={(e) => setFromName(e.target.value)}
                  placeholder="Agentic Marketing Agency"
                />
              </Field>
              <Field label="From email" id="smtp-fromemail">
                <Input
                  id="smtp-fromemail"
                  data-testid="smtp-from-email"
                  value={fromEmail}
                  onChange={(e) => setFromEmail(e.target.value)}
                  placeholder="hello@mail.acme.com"
                />
              </Field>
            </div>

            {testUnwired ? (
              <Alert data-testid="smtp-test-unwired">
                <TriangleAlert className="size-4" />
                <AlertDescription>
                  This host was built without the <code>smtp</code> feature, so it can&rsquo;t
                  send mail — the credentials above are stored and will be used by a build
                  that has it.
                </AlertDescription>
              </Alert>
            ) : null}

            <div className="flex items-center gap-2">
              <Button onClick={() => void save()} disabled={busy} data-testid="smtp-save">
                {busy ? <Loader2 className="mr-2 size-4 animate-spin" /> : null}
                Save
              </Button>
              <Button
                variant="outline"
                disabled={busy || !testable || testUnwired}
                onClick={() => void test()}
                data-testid="smtp-test"
              >
                <ShieldAlert className="size-4" /> Test connection
              </Button>
              {testable ? null : (
                <span className="text-xs text-muted-foreground" data-testid="smtp-test-hint">
                  Save a complete configuration, password included, to test it.
                </span>
              )}
            </div>
          </>
        )}
      </CardContent>
    </Card>
  );
}

function Field({
  label,
  id,
  hint,
  children,
}: {
  label: string;
  id: string;
  /** Rendered under the control. Use it to state what happens to what is typed. */
  hint?: string;
  children: React.ReactNode;
}) {
  return (
    <div className={cn("grid gap-2")}>
      <Label htmlFor={id}>{label}</Label>
      {children}
      {hint ? <p className="text-xs text-muted-foreground">{hint}</p> : null}
    </div>
  );
}

/** A field a member reads but cannot edit. Same `id` an editable form uses for it. */
function ReadOnlyField({ label, id, value }: { label: string; id: string; value?: string }) {
  return (
    <div className="grid gap-2">
      <Label htmlFor={id}>{label}</Label>
      <p id={id} data-testid={id} className="text-sm">
        {value || <span className="text-muted-foreground">Not set</span>}
      </p>
    </div>
  );
}
