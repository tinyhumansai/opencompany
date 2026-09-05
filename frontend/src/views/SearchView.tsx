import { useCallback, useEffect, useState } from "react";
import { Check, Loader2, TriangleAlert } from "lucide-react";
import { toast } from "sonner";

import { clearSearch, getSearch, saveSearch, type SearchStatus } from "@/api/search";
import type { OpenCompanyClient } from "@/api/client";
import { AdminOnlyNotice } from "@/components/admin-only-notice";
import { PageHeader } from "@/components/page-header";
import { useCanManage } from "@/hooks/use-can-manage";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { GrantNamespace } from "@/components/grant-namespace";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";

interface Props {
  client: OpenCompanyClient;
  company: string | null;
}

/** What each provider is called, and where its key comes from. */
const PROVIDERS: Record<string, { label: string; help: string }> = {
  managed: {
    label: "Managed (included)",
    help: "Search through the platform's own account. Nothing to configure, and every call is metered against this company's daily allowance.",
  },
  brave: {
    label: "Brave Search",
    help: "Create a key at brave.com/search/api. Adds news, image and video search beside the web search.",
  },
  exa: {
    label: "Exa",
    help: "Create a key at exa.ai. Adds find-similar and full page contents beside the web search.",
  },
  querit: {
    label: "Querit",
    help: "Create a key with your Querit account.",
  },
  searxng: {
    label: "SearXNG (self-hosted)",
    help: "Your own SearXNG instance. No account and no key — just the address it answers on.",
  },
};

/** The message out of a rejected request, whatever it was rejected with. */
function reason(err: unknown): string {
  return err instanceof Error ? err.message : String(err);
}

/**
 * Settings → Search: which index this company's teammates search.
 *
 * Every teammate that holds the `search` grant gets a `web_search` tool. This
 * page decides what is behind it: the platform's own account (metered, capped,
 * nothing to configure) or the company's own search provider.
 *
 * # Why "selected" and "effective" are two different fields
 *
 * Picking Exa and not pasting a key is not a connection. The teammates keep
 * searching through the managed surface, and a page that showed only the
 * selection would be reporting an account that is never called. So the badge
 * reads off `effectiveProvider`, and the selection is shown as a selection.
 *
 * # Two problems this form cannot fix
 *
 *   - not granted  — a key is stored and STILL nothing reaches a teammate,
 *                    because the manifest does not grant `search`. The fix is
 *                    `company.toml`, not this page.
 *   - not in build — the running host was compiled without the agent harness,
 *                    so no amount of configuring will do anything.
 *
 * Both are said above the form, so an operator does not fill it in and wonder
 * why nothing happened.
 *
 * # The key is write-only
 *
 * It is never returned by the host, so it is never rendered. A stored key shows
 * as "Configured" and the input stays empty with a placeholder saying that
 * typing replaces it.
 */
export function SearchView({ client, company }: Props) {
  const [status, setStatus] = useState<SearchStatus | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  // Whether this viewer may change where this company's teammates search.
  //
  // Governs the whole page, not just the grant control: `PUT …/search` and
  // `DELETE …/search/key` are both `AdminScopedCompany`. The footnote at the
  // bottom of this page has always said the choice is an administrator's; until
  // this gate existed, the page said that under an enabled provider picker.
  const canManage = useCanManage(client, company);

  const [provider, setProvider] = useState("managed");
  const [apiKey, setApiKey] = useState("");
  const [endpoint, setEndpoint] = useState("");

  // Every piece of state here belongs to ONE company — including the
  // typed-but-unsaved API key. `SettingsSection` renders this with
  // `key={company}` so a company switch remounts rather than carrying a key
  // typed for one company into another company's Save.
  const load = useCallback(async () => {
    try {
      const next = await getSearch(client, company);
      setStatus(next);
      setLoadError(null);
      setProvider(next.provider);
      // Seed the endpoint with what is stored — it is the one non-secret field,
      // and an operator correcting a typo should not have to retype it.
      setEndpoint(next.endpoint ?? "");
    } catch (err) {
      setLoadError(reason(err));
    }
  }, [client, company]);

  useEffect(() => {
    void load();
  }, [load]);

  async function onSave() {
    // Send only what changed or was actually entered. The host treats this as a
    // patch, so an untouched key keeps its stored value rather than being
    // cleared.
    const body: Record<string, string> = {};
    if (provider && provider !== status?.provider) body.provider = provider;
    if (apiKey.trim()) body.apiKey = apiKey.trim();
    if (endpoint.trim() && endpoint.trim() !== (status?.endpoint ?? "")) {
      body.endpoint = endpoint.trim();
    }

    if (Object.keys(body).length === 0) {
      toast.info("Nothing to save — change a field first.");
      return;
    }

    setBusy(true);
    try {
      const next = await saveSearch(client, company, body);
      setStatus(next);
      setProvider(next.provider);
      // Clear the secret input on success: leaving a key sitting in a form field
      // after it has been stored is one stray screen-share from a leak.
      setApiKey("");
      toast.success("Search settings saved.");
    } catch (err) {
      toast.error(reason(err));
    } finally {
      setBusy(false);
    }
  }

  async function onClear() {
    setBusy(true);
    try {
      const next = await clearSearch(client, company);
      setStatus(next);
      setProvider(next.provider);
      setApiKey("");
      setEndpoint("");
      toast.success("Back to managed search.");
    } catch (err) {
      toast.error(reason(err));
    } finally {
      setBusy(false);
    }
  }

  /*
    Hoisted above the state conditionals (codex review, #1785). Both early
    returns used to run before the header, so the page had no `h1` while it
    loaded and — because the read is not retried — none at all once it failed.
    The error state is the one that matters: it is terminal, so a screen reader
    got a page with no accessible name and no way out of it.

    Read once into a const rather than duplicated into three returns: three
    copies of a page's own name is how the console got twelve of them.
  */
  const header = (
    <PageHeader
      title="Search"
      width="5xl"
      description={
        <>
          Where your teammates look things up. Every teammate that can search
          gets one <code>web_search</code> tool — this decides which index
          answers it, and whose account pays for the call.
        </>
      }
    />
  );

  if (loadError) {
    return (
      <div className="flex min-h-0 flex-1 flex-col">
        {header}
        <div className="mx-auto w-full max-w-5xl px-4 py-6">
          <Alert variant="destructive" data-testid="search-load-error">
            <TriangleAlert className="size-4" />
            <AlertDescription>Could not load search settings: {loadError}</AlertDescription>
          </Alert>
        </div>
      </div>
    );
  }

  if (!status) {
    return (
      <div className="flex min-h-0 flex-1 flex-col">
        {header}
        <div className="flex flex-1 items-center justify-center text-sm text-muted-foreground">
          <Loader2 className="mr-2 size-4 animate-spin" /> Loading search…
        </div>
      </div>
    );
  }

  const byo = provider !== "managed";
  const labels = Object.fromEntries(
    status.supportedProviders.map((slug) => [slug, PROVIDERS[slug]?.label ?? slug]),
  );
  const connected = status.inBuild && status.granted && status.effectiveProvider !== "managed";

  return (
    <div className="flex min-h-0 flex-1 flex-col" data-testid="search-view">
      {header}
      <div className="mx-auto min-h-0 w-full max-w-5xl flex-1 space-y-6 overflow-y-auto px-4 py-6">

        {!canManage && (
          <AdminOnlyNotice
            testId="search-read-only"
            title="Only an admin can change where this company searches"
          >
            Whatever a teammate types into a search reaches the provider selected
            here, under that provider&rsquo;s own retention policy &mdash; and the
            calls are billed to whichever account the key belongs to. Both are the
            company&rsquo;s to decide, so an admin decides them. You can see which
            index answers today.
          </AdminOnlyNotice>
        )}

        {!status.inBuild ? (
          <Alert data-testid="search-not-in-build">
            <TriangleAlert className="size-4" />
            <AlertDescription>
              This host was built without the agent tools, so these settings will
              be stored and have no effect. Rebuild with the{" "}
              <code>openhuman</code> feature.
            </AlertDescription>
          </Alert>
        ) : null}

        {status.inBuild && !status.granted ? (
          <GrantNamespace
            client={client}
            company={company}
            namespace="search"
            canManage={canManage}
            explanation="No teammate will get a search tool even once a provider is configured."
            onGranted={load}
            testId="search-not-granted"
          />
        ) : null}

        <Card>
          <CardContent className="space-y-4">
            <div className="flex items-center justify-between">
              <div className="space-y-1">
                <p className="text-sm font-medium">
                  {PROVIDERS[status.effectiveProvider]?.label ?? status.effectiveProvider}
                </p>
                <p className="text-xs text-muted-foreground">
                  {status.effectiveProvider === "managed"
                    ? status.needsApiKey || status.needsEndpoint
                      ? `${PROVIDERS[status.provider]?.label ?? status.provider} is selected but not finished, so searches still go through the included account.`
                      : "Searching through the included account, metered against this company's daily allowance."
                    : "Searching through this company's own account."}
                </p>
              </div>
              {connected ? (
                <Badge variant="secondary" data-testid="search-connected">
                  <Check className="mr-1 size-3" /> Own account
                </Badge>
              ) : null}
            </div>

            <div className="grid gap-4 sm:grid-cols-2">
              <div className="space-y-2">
                <Label htmlFor="search-provider">Provider</Label>
                <Select
                  value={provider}
                  onValueChange={(v) => v && setProvider(String(v))}
                  items={labels}
                  disabled={!canManage}
                >
                  <SelectTrigger id="search-provider" data-testid="search-provider">
                    <SelectValue />
                  </SelectTrigger>
                  <SelectContent>
                    {status.supportedProviders.map((slug) => (
                      <SelectItem key={slug} value={slug}>
                        {PROVIDERS[slug]?.label ?? slug}
                      </SelectItem>
                    ))}
                  </SelectContent>
                </Select>
                <p className="text-xs text-muted-foreground">
                  {PROVIDERS[provider]?.help ?? ""}
                </p>
              </div>

              {/* Withheld from a member, not disabled — the same rule the API
                token follows on the Hosting page. A disabled password box is
                still somewhere to aim a paste. */}
              {canManage && byo && provider !== "searxng" ? (
                <div className="space-y-2">
                  <Label htmlFor="search-key">API key</Label>
                  <Input
                    id="search-key"
                    data-testid="search-api-key"
                    type="password"
                    autoComplete="off"
                    placeholder={
                      status.apiKeyConfigured ? "Configured — type to replace" : "sk_…"
                    }
                    value={apiKey}
                    onChange={(e) => setApiKey(e.target.value)}
                  />
                  <p className="text-xs text-muted-foreground">
                    Stored write-only: it is never shown again, here or anywhere
                    else. Searches are billed to this account, not to us.
                  </p>
                </div>
              ) : null}

              {provider === "searxng" ? (
                <div className="space-y-2">
                  <Label htmlFor="search-endpoint">Instance URL</Label>
                  <Input
                    id="search-endpoint"
                    data-testid="search-endpoint"
                    placeholder="https://searx.example.com"
                    value={endpoint}
                    disabled={!canManage}
                    onChange={(e) => setEndpoint(e.target.value)}
                  />
                  <p className="text-xs text-muted-foreground">
                    The address your SearXNG instance answers on. Every teammate
                    search goes there, so it has to be reachable from this host.
                  </p>
                </div>
              ) : null}
            </div>

            <div className="flex items-center gap-2">
              {canManage ? (
                <Button onClick={() => void onSave()} disabled={busy} data-testid="search-save">
                  {busy ? <Loader2 className="mr-2 size-4 animate-spin" /> : null}
                  Save
                </Button>
              ) : null}
              {canManage && status.provider !== "managed" ? (
                <Button
                  variant="outline"
                  onClick={() => void onClear()}
                  disabled={busy}
                  data-testid="search-clear"
                >
                  Use managed search
                </Button>
              ) : null}
            </div>
          </CardContent>
        </Card>

        <p className="text-xs text-muted-foreground">
          Search queries leave this host. Whatever a teammate types into a search
          reaches the provider selected here, under that provider&rsquo;s own
          retention policy — which is the reason the choice is an
          administrator&rsquo;s and not a teammate&rsquo;s.
        </p>
      </div>
    </div>
  );
}
