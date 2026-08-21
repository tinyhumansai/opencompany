import { useCallback, useEffect, useState } from "react";
import { Check, Copy, Globe, Info, Loader2, Mail, ShieldAlert } from "lucide-react";
import { toast } from "sonner";

import type { OpenCompanyClient } from "@/api/client";
import {
  type DnsRecord,
  type DomainStatus,
  fetchMailStatus,
  putDomain,
  putSmtp,
  type SmtpSecurity,
  type SmtpStatus,
  testSmtp,
  verifyDomain,
} from "@/api/domain";
import { ApiError } from "@/api/types";
import { isValidDomain, parseSmtpPort } from "@/lib/domain";
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
import { Skeleton } from "@/components/ui/skeleton";
import { cn } from "@/lib/utils";

interface Props {
  client: OpenCompanyClient;
  company: string | null;
}

const SECURITY_LABELS: Record<SmtpSecurity, string> = {
  none: "None",
  starttls: "STARTTLS",
  ssl: "SSL / TLS",
};

type Load = "loading" | "ready" | "error";

/**
 * Custom domain (with host-issued DNS records) and the company's own SMTP
 * credentials (issue #1460).
 *
 * Everything here is the host's: status is read over GraphQL, the domain and its
 * records are set through `PUT …/domain`, verification runs on the host through
 * `POST …/domain/verify`, and SMTP credentials are stored write-only through
 * `PUT …/smtp`. Nothing is kept in the browser — the SMTP password is held in
 * component state only, never persisted, and the DNS records come from the host
 * rather than being fabricated client-side.
 */
export function DomainSettings({ client, company }: Props) {
  const [load, setLoad] = useState<Load>("loading");
  const [domain, setDomain] = useState<DomainStatus | null>(null);
  const [smtp, setSmtp] = useState<SmtpStatus | null>(null);

  const refresh = useCallback(async () => {
    try {
      const status = await fetchMailStatus(client, company);
      setDomain(status.domain);
      setSmtp(status.smtp);
      setLoad("ready");
    } catch {
      // The host could not answer the mail-status read. Say so rather than
      // rendering an empty "nothing configured" form over unknown state.
      setLoad("error");
    }
  }, [client, company]);

  useEffect(() => {
    setLoad("loading");
    void refresh();
  }, [refresh]);

  if (load === "loading") {
    return (
      <div className="space-y-4">
        <Skeleton className="h-40 rounded-xl" />
        <Skeleton className="h-64 rounded-xl" />
      </div>
    );
  }

  if (load === "error") {
    return (
      <Card>
        <CardContent className="py-4">
          <p className="text-xs text-muted-foreground">
            Couldn&apos;t read this company&apos;s mail settings — the host could not answer, so this
            is unknown rather than unconfigured. Reload to try again.
          </p>
        </CardContent>
      </Card>
    );
  }

  return (
    <>
      <DomainCard client={client} company={company} domain={domain} onChange={setDomain} />
      <SmtpCard client={client} company={company} smtp={smtp} onChange={setSmtp} />
    </>
  );
}

function DomainCard({
  client,
  company,
  domain,
  onChange,
}: {
  client: OpenCompanyClient;
  company: string | null;
  domain: DomainStatus | null;
  onChange: (status: DomainStatus) => void;
}) {
  const [draft, setDraft] = useState("");
  const [busy, setBusy] = useState<"save" | "verify" | "remove" | null>(null);
  const configured = domain !== null && domain.domain !== "";

  async function connect() {
    const value = draft.trim().toLowerCase();
    if (!isValidDomain(value)) {
      toast.error("Enter a valid domain, e.g. mail.acme.com");
      return;
    }
    setBusy("save");
    try {
      // The records come back from the host — the console no longer invents
      // them, so an operator publishes what this deployment actually chose.
      onChange(await putDomain(client, company, value));
      setDraft("");
      toast.success("Domain saved — add the DNS records below.");
    } catch (err) {
      toast.error(err instanceof ApiError ? err.message : "Couldn't save the domain.");
    } finally {
      setBusy(null);
    }
  }

  async function verify() {
    setBusy("verify");
    try {
      const status = await verifyDomain(client, company);
      onChange(status);
      if (status.verified) {
        toast.success("Domain verified.");
      } else {
        toast.message("Records not found yet — DNS changes can take up to 48h to propagate.");
      }
    } catch (err) {
      toast.error(
        err instanceof ApiError && err.code === "not_wired"
          ? "DNS verification isn't enabled on this host — rebuild it with the `dns` feature."
          : err instanceof ApiError
            ? err.message
            : "Couldn't verify the domain.",
      );
    } finally {
      setBusy(null);
    }
  }

  async function remove() {
    setBusy("remove");
    try {
      // No delete route: clearing the domain is an empty set, which the host
      // stores as "no domain" and answers with an empty record list.
      onChange(await putDomain(client, company, ""));
      setDraft("");
    } catch (err) {
      toast.error(err instanceof ApiError ? err.message : "Couldn't remove the domain.");
    } finally {
      setBusy(null);
    }
  }

  return (
    <Card>
      <CardHeader>
        <CardTitle className="flex items-center gap-2 text-base">
          <Globe className="size-4" /> Custom domain
        </CardTitle>
        <CardDescription>Send and receive on your own domain instead of the default.</CardDescription>
      </CardHeader>
      <CardContent className="space-y-4">
        {!configured ? (
          <div className="flex flex-col gap-2 sm:flex-row">
            <Input
              value={draft}
              onChange={(e) => setDraft(e.target.value)}
              placeholder="mail.acme.com"
              onKeyDown={(e) => e.key === "Enter" && void connect()}
            />
            <Button className="shrink-0" disabled={busy !== null} onClick={() => void connect()}>
              {busy === "save" ? <Loader2 className="size-4 animate-spin" /> : null}
              Add domain
            </Button>
          </div>
        ) : (
          <>
            <div className="flex flex-wrap items-center justify-between gap-2 rounded-lg border p-3">
              <span className="inline-flex items-center gap-2 font-mono text-sm">
                <Globe className="size-4 text-muted-foreground" />
                {domain.domain}
              </span>
              <div className="flex items-center gap-2">
                {domain.verified ? (
                  <Badge className="gap-1 bg-status-done-soft text-status-done-text">
                    <Check className="size-3" /> Verified
                  </Badge>
                ) : (
                  <Badge variant="secondary" className="gap-1">
                    <span className="size-1.5 rounded-full bg-status-blocked" /> Pending
                  </Badge>
                )}
                <Button variant="ghost" size="sm" disabled={busy !== null} onClick={() => void remove()}>
                  {busy === "remove" ? <Loader2 className="size-4 animate-spin" /> : null}
                  Remove
                </Button>
              </div>
            </div>

            <div className="space-y-2">
              <p className="text-sm font-medium">Add these DNS records</p>
              <DnsTable records={domain.records} />
              <div className="flex items-center gap-2 pt-1">
                <Button
                  variant="outline"
                  size="sm"
                  disabled={busy !== null}
                  onClick={() => void verify()}
                >
                  {busy === "verify" ? <Loader2 className="size-4 animate-spin" /> : null}
                  Verify DNS
                </Button>
                <p className="text-xs text-muted-foreground">Changes can take up to 48h to propagate.</p>
              </div>
            </div>
          </>
        )}
      </CardContent>
    </Card>
  );
}

function DnsTable({ records }: { records: DnsRecord[] }) {
  if (records.length === 0) {
    return (
      <p className="rounded-lg border p-3 text-xs text-muted-foreground">
        The host has not issued any records for this domain yet.
      </p>
    );
  }
  return (
    <div className="overflow-x-auto rounded-lg border">
      <table className="w-full text-left text-xs">
        <thead className="bg-muted/50 text-muted-foreground">
          <tr>
            <th className="px-3 py-2 font-medium">Type</th>
            <th className="px-3 py-2 font-medium">Name</th>
            <th className="px-3 py-2 font-medium">Value</th>
            <th className="px-3 py-2 font-medium">TTL</th>
          </tr>
        </thead>
        <tbody className="divide-y">
          {records.map((r, i) => (
            <tr key={i} className="align-top">
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
            </tr>
          ))}
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
    <button onClick={copy} className="group flex max-w-[28ch] items-center gap-1.5 text-left sm:max-w-[40ch]" title="Copy">
      <span className="truncate font-mono">{value}</span>
      {copied ? (
        <Check className="size-3 shrink-0 text-status-done-text" />
      ) : (
        <Copy className="size-3 shrink-0 text-muted-foreground opacity-0 transition-opacity group-hover:opacity-100" />
      )}
    </button>
  );
}

function SmtpCard({
  client,
  company,
  smtp,
  onChange,
}: {
  client: OpenCompanyClient;
  company: string | null;
  smtp: SmtpStatus | null;
  onChange: (status: SmtpStatus) => void;
}) {
  // The card mounts after the status read, so the non-secret fields the host
  // returns (host / port / username) initialise the form once. The password is
  // never returned and never stored — it lives here in memory only, and a save
  // sends it write-only to the host's secret store.
  const [host, setHost] = useState(smtp?.host ?? "");
  const [port, setPort] = useState(smtp && smtp.port > 0 ? String(smtp.port) : "587");
  const [security, setSecurity] = useState<SmtpSecurity>("starttls");
  const [username, setUsername] = useState(smtp?.username ?? "");
  const [password, setPassword] = useState("");
  const [fromName, setFromName] = useState("");
  const [fromEmail, setFromEmail] = useState("");
  const [busy, setBusy] = useState<"save" | "test" | null>(null);

  const configured = smtp?.configured === true;
  // A save REPLACES the stored credential in full — the host has no partial
  // update — so the password is required every time, not only on first setup.
  const canSave =
    host.trim() !== "" &&
    parseSmtpPort(port) !== null &&
    username.trim() !== "" &&
    fromEmail.trim() !== "" &&
    password !== "";

  async function save() {
    const parsedPort = parseSmtpPort(port);
    if (parsedPort === null) {
      toast.error("Enter a valid port (1–65535).");
      return;
    }
    setBusy("save");
    try {
      const status = await putSmtp(client, company, {
        host: host.trim(),
        port: parsedPort,
        security,
        username: username.trim(),
        password,
        from_name: fromName.trim(),
        from_email: fromEmail.trim(),
      });
      onChange(status);
      // Never keep the secret around after it has gone to the host.
      setPassword("");
      toast.success("SMTP credentials saved.");
    } catch (err) {
      toast.error(err instanceof ApiError ? err.message : "Couldn't save the SMTP credentials.");
    } finally {
      setBusy(null);
    }
  }

  async function test() {
    setBusy("test");
    try {
      const result = await testSmtp(client, company);
      if (result.ok) {
        toast.success(result.message);
      } else {
        toast.error(result.message);
      }
    } catch (err) {
      toast.error(
        err instanceof ApiError && err.code === "not_wired"
          ? "Test send isn't enabled on this host — rebuild it with the `smtp` feature."
          : err instanceof ApiError
            ? err.message
            : "Couldn't send the test email.",
      );
    } finally {
      setBusy(null);
    }
  }

  return (
    <Card>
      <CardHeader>
        <CardTitle className="flex items-center gap-2 text-base">
          <Mail className="size-4" /> Email (SMTP)
          {configured && (
            <Badge variant="secondary" className="gap-1">
              <Check className="size-3" /> Configured
            </Badge>
          )}
        </CardTitle>
        <CardDescription>The outbound mail server your company sends through.</CardDescription>
      </CardHeader>
      <CardContent className="space-y-4">
        <div className="grid gap-4 sm:grid-cols-2">
          <Field label="SMTP host" id="smtp-host">
            <Input id="smtp-host" value={host} onChange={(e) => setHost(e.target.value)} placeholder="smtp.postmarkapp.com" />
          </Field>
          <div className="grid grid-cols-2 gap-3">
            <Field label="Port" id="smtp-port">
              <Input id="smtp-port" value={port} onChange={(e) => setPort(e.target.value)} placeholder="587" inputMode="numeric" />
            </Field>
            <Field label="Security" id="smtp-security">
              <Select value={security} onValueChange={(v) => v && setSecurity(v as SmtpSecurity)}>
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
            <Input id="smtp-user" value={username} onChange={(e) => setUsername(e.target.value)} placeholder="apikey" autoComplete="off" />
          </Field>
          <Field label={configured ? "Password (stored — enter to replace)" : "Password"} id="smtp-pass">
            <Input
              id="smtp-pass"
              type="password"
              value={password}
              onChange={(e) => setPassword(e.target.value)}
              placeholder={configured ? "•••••• write-only" : "••••••••"}
              autoComplete="off"
            />
          </Field>
          <Field label="From name" id="smtp-fromname">
            <Input id="smtp-fromname" value={fromName} onChange={(e) => setFromName(e.target.value)} placeholder="Agentic Marketing Agency" />
          </Field>
          <Field label="From email" id="smtp-fromemail">
            <Input id="smtp-fromemail" value={fromEmail} onChange={(e) => setFromEmail(e.target.value)} placeholder="hello@mail.acme.com" />
          </Field>
        </div>

        <Alert>
          <Info className="size-4" />
          <AlertDescription>
            Stored write-only in the host&apos;s secret store and used per tenant — the password is
            never shown again, and a save replaces the whole credential. A change takes effect on the
            next send.
          </AlertDescription>
        </Alert>

        <div className="flex flex-wrap items-center gap-2">
          <Button disabled={busy !== null || !canSave} onClick={() => void save()}>
            {busy === "save" ? <Loader2 className="size-4 animate-spin" /> : <Check className="size-4" />}
            Save
          </Button>
          <Button
            variant="outline"
            disabled={busy !== null || !configured}
            onClick={() => void test()}
          >
            {busy === "test" ? (
              <Loader2 className="size-4 animate-spin" />
            ) : (
              <ShieldAlert className="size-4" />
            )}
            Test connection
          </Button>
          {!configured && (
            <span className="text-xs text-muted-foreground">Save credentials before sending a test.</span>
          )}
        </div>
      </CardContent>
    </Card>
  );
}

function Field({ label, id, children }: { label: string; id: string; children: React.ReactNode }) {
  return (
    <div className={cn("grid gap-2")}>
      <Label htmlFor={id}>{label}</Label>
      {children}
    </div>
  );
}
