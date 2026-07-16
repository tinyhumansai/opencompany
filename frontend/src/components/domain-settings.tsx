import { useEffect, useMemo, useState } from "react";
import { Check, Copy, Globe, Info, Mail, ShieldAlert } from "lucide-react";
import { toast } from "sonner";

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
import { cn } from "@/lib/utils";
import {
  type DnsRecord,
  dnsRecords,
  isValidDomain,
  loadMailSettings,
  type MailSettings,
  saveMailSettings,
  type SmtpSecurity,
} from "@/lib/domain";

interface Props {
  company: string | null;
}

const SECURITY_LABELS: Record<SmtpSecurity, string> = {
  none: "None",
  starttls: "STARTTLS",
  ssl: "SSL / TLS",
};

/** Custom domain (with DNS records) and SMTP credentials for the company. */
export function DomainSettings({ company }: Props) {
  const [settings, setSettings] = useState<MailSettings>(() => loadMailSettings(company));

  useEffect(() => {
    saveMailSettings(company, settings);
  }, [company, settings]);

  return (
    <>
      <DomainCard settings={settings} setSettings={setSettings} />
      <SmtpCard settings={settings} setSettings={setSettings} />
    </>
  );
}

function DomainCard({
  settings,
  setSettings,
}: {
  settings: MailSettings;
  setSettings: React.Dispatch<React.SetStateAction<MailSettings>>;
}) {
  const [draft, setDraft] = useState(settings.domain.domain);
  const configured = Boolean(settings.domain.domain);
  const records = useMemo(() => dnsRecords(settings.domain.domain), [settings.domain.domain]);

  function connect() {
    const domain = draft.trim().toLowerCase();
    if (!isValidDomain(domain)) {
      toast.error("Enter a valid domain, e.g. mail.acme.com");
      return;
    }
    setSettings((s) => ({ ...s, domain: { domain, verified: false } }));
    toast.success("Domain saved — add the DNS records below.");
  }

  function remove() {
    setSettings((s) => ({ ...s, domain: { domain: "", verified: false } }));
    setDraft("");
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
              onKeyDown={(e) => e.key === "Enter" && connect()}
            />
            <Button className="shrink-0" onClick={connect}>
              Add domain
            </Button>
          </div>
        ) : (
          <>
            <div className="flex flex-wrap items-center justify-between gap-2 rounded-lg border p-3">
              <span className="inline-flex items-center gap-2 font-mono text-sm">
                <Globe className="size-4 text-muted-foreground" />
                {settings.domain.domain}
              </span>
              <div className="flex items-center gap-2">
                {settings.domain.verified ? (
                  <Badge className="gap-1 bg-emerald-500/15 text-emerald-600 dark:text-emerald-400">
                    <Check className="size-3" /> Verified
                  </Badge>
                ) : (
                  <Badge variant="secondary" className="gap-1">
                    <span className="size-1.5 animate-pulse rounded-full bg-amber-500" /> Pending
                  </Badge>
                )}
                <Button variant="ghost" size="sm" onClick={remove}>
                  Remove
                </Button>
              </div>
            </div>

            <div className="space-y-2">
              <p className="text-sm font-medium">Add these DNS records</p>
              <DnsTable records={records} />
              <div className="flex items-center gap-2 pt-1">
                <Button
                  variant="outline"
                  size="sm"
                  onClick={() => toast.info("DNS verification runs on the host once connected. Records are saved.")}
                >
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
        <Check className="size-3 shrink-0 text-emerald-500" />
      ) : (
        <Copy className="size-3 shrink-0 text-muted-foreground opacity-0 transition-opacity group-hover:opacity-100" />
      )}
    </button>
  );
}

function SmtpCard({
  settings,
  setSettings,
}: {
  settings: MailSettings;
  setSettings: React.Dispatch<React.SetStateAction<MailSettings>>;
}) {
  const s = settings.smtp;
  const set = (patch: Partial<typeof s>) =>
    setSettings((prev) => ({ ...prev, smtp: { ...prev.smtp, ...patch } }));

  const complete = s.host && s.port && s.username && s.fromEmail;

  return (
    <Card>
      <CardHeader>
        <CardTitle className="flex items-center gap-2 text-base">
          <Mail className="size-4" /> Email (SMTP)
        </CardTitle>
        <CardDescription>The outbound mail server your company sends through.</CardDescription>
      </CardHeader>
      <CardContent className="space-y-4">
        <div className="grid gap-4 sm:grid-cols-2">
          <Field label="SMTP host" id="smtp-host">
            <Input id="smtp-host" value={s.host} onChange={(e) => set({ host: e.target.value })} placeholder="smtp.postmarkapp.com" />
          </Field>
          <div className="grid grid-cols-2 gap-3">
            <Field label="Port" id="smtp-port">
              <Input id="smtp-port" value={s.port} onChange={(e) => set({ port: e.target.value })} placeholder="587" inputMode="numeric" />
            </Field>
            <Field label="Security" id="smtp-security">
              <Select
                value={s.security}
                onValueChange={(v) => v && set({ security: v as SmtpSecurity })}
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
            <Input id="smtp-user" value={s.username} onChange={(e) => set({ username: e.target.value })} placeholder="apikey" autoComplete="off" />
          </Field>
          <Field label="Password" id="smtp-pass">
            <Input id="smtp-pass" type="password" value={s.password} onChange={(e) => set({ password: e.target.value })} placeholder="••••••••" autoComplete="off" />
          </Field>
          <Field label="From name" id="smtp-fromname">
            <Input id="smtp-fromname" value={s.fromName} onChange={(e) => set({ fromName: e.target.value })} placeholder="Agentic Marketing Agency" />
          </Field>
          <Field label="From email" id="smtp-fromemail">
            <Input id="smtp-fromemail" value={s.fromEmail} onChange={(e) => set({ fromEmail: e.target.value })} placeholder="hello@mail.acme.com" />
          </Field>
        </div>

        <Alert>
          <Info className="size-4" />
          <AlertDescription>
            Saved to this browser as a draft. When the host is connected, credentials are stored in
            its secret store and used per tenant — never handed to the workload directly.
          </AlertDescription>
        </Alert>

        <div className="flex items-center gap-2">
          <Button
            variant="outline"
            disabled={!complete}
            onClick={() => toast.info("A test email is sent from the host once SMTP is connected.")}
          >
            <ShieldAlert className="size-4" /> Test connection
          </Button>
          {complete ? (
            <span className="inline-flex items-center gap-1 text-xs text-emerald-600 dark:text-emerald-400">
              <Check className="size-3.5" /> Ready
            </span>
          ) : (
            <span className="text-xs text-muted-foreground">Fill host, port, username, and from email.</span>
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
