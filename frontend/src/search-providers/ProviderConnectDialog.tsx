import { useEffect, useState } from "react";

import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { COPY, entry } from "./catalogue";

/** What the dialog is being opened for. */
export type ConnectIntent =
  | { kind: "connect"; slug: string }
  | { kind: "replace-key"; slug: string }
  | { kind: "edit-endpoint"; slug: string; endpoint: string | null };

/**
 * Connect a provider, replace its key, or re-address it.
 *
 * One dialog with two fields rather than two dialogs, because the fields are
 * decided by the provider's kind and never by both at once: an account provider
 * has a key and no address, a self-hosted instance has an address and no
 * account. `takesKey`/`takesEndpoint` come from the host, so the console never
 * re-derives which is which.
 *
 * ## No Name field, no slug line
 *
 * The LLM dialog derives a slug from a name the operator types, because an
 * operator may connect two accounts at the same provider. Here the slug is the
 * catalogue slug — the harness dispatches on it — so there is at most one row
 * per provider and nothing to name. A select over one option is a button wearing
 * a costume, and a Name field over one possible value is the same mistake.
 *
 * ## The cost warning sits on the button it applies to
 *
 * No hosted search provider publishes a free credential validator, so confirming
 * a good key costs one real query. SearXNG's check is free and says nothing.
 */
export function ProviderConnectDialog({
  intent,
  busy,
  onCancel,
  onSubmit,
}: {
  intent: ConnectIntent | null;
  busy: boolean;
  onCancel: () => void;
  onSubmit: (values: { apiKey?: string; endpoint?: string }) => void;
}) {
  const item = intent ? entry(intent.slug) : undefined;
  const [apiKey, setApiKey] = useState("");
  const [endpoint, setEndpoint] = useState("");

  // Every field here belongs to ONE attempt. Re-opening the dialog for another
  // provider must not carry a key typed for the previous one into this save.
  useEffect(() => {
    setApiKey("");
    setEndpoint(
      intent?.kind === "edit-endpoint" ? (intent.endpoint ?? "") : "",
    );
  }, [intent]);

  if (!intent || !item) return null;

  const wantsKey =
    item.category === "account" && intent.kind !== "edit-endpoint";
  const wantsEndpoint =
    item.category === "self-hosted" && intent.kind !== "replace-key";
  const ready =
    (!wantsKey || apiKey.trim().length > 0) &&
    (!wantsEndpoint || endpoint.trim().length > 0);

  const title =
    intent.kind === "connect"
      ? `Connect ${item.label}`
      : intent.kind === "replace-key"
        ? `Replace the ${item.label} key`
        : `${item.label} instance address`;

  return (
    <Dialog open onOpenChange={(open) => !open && onCancel()}>
      <DialogContent
        className="sm:max-w-md"
        data-testid="search-connect-provider"
      >
        <DialogHeader>
          <DialogTitle>{title}</DialogTitle>
          <DialogDescription>
            {wantsKey
              ? "The key is stored on this company and never shown again."
              : "The address of your own instance. There is no account and no key."}
          </DialogDescription>
        </DialogHeader>

        {/* A real form, so Enter in either field submits. A dialog whose only
          route to its primary action is a mouse is one a keyboard user has to
          tab past every field to finish. */}
        <form
          onSubmit={(event) => {
            event.preventDefault();
            if (!busy && ready) {
              onSubmit({
                apiKey: wantsKey ? apiKey.trim() : undefined,
                endpoint: wantsEndpoint ? endpoint.trim() : undefined,
              });
            }
          }}
        >
          <div className="grid gap-4">
            {wantsKey && (
              <div className="grid gap-1.5">
                <Label htmlFor="search-connect-key">API key</Label>
                <Input
                  id="search-connect-key"
                  type="password"
                  autoComplete="off"
                  maxLength={512}
                  value={apiKey}
                  data-testid="search-connect-key"
                  onChange={(event) => setApiKey(event.target.value)}
                />
                {item.keyHint && (
                  <p className="text-xs text-muted-foreground">
                    {item.keyHint}
                  </p>
                )}
              </div>
            )}

            {wantsEndpoint && (
              <div className="grid gap-1.5">
                <Label htmlFor="search-connect-endpoint">Instance URL</Label>
                <Input
                  id="search-connect-endpoint"
                  inputMode="url"
                  placeholder="https://search.example.internal"
                  maxLength={2048}
                  value={endpoint}
                  data-testid="search-connect-endpoint"
                  onChange={(event) => setEndpoint(event.target.value)}
                />
                <p className="text-xs text-muted-foreground">
                  {COPY.searxngFormats}
                </p>
              </div>
            )}
          </div>

          <DialogFooter className="gap-2 sm:flex-col sm:items-stretch">
            {/* On the control it applies to, rather than above the fold. */}
            {wantsKey && (
              <p className="text-xs text-muted-foreground">{COPY.checkCosts}</p>
            )}
            <div className="flex justify-end gap-2">
              <Button
                type="button"
                variant="outline"
                disabled={busy}
                onClick={onCancel}
              >
                Cancel
              </Button>
              <Button
                type="submit"
                disabled={busy || !ready}
                data-testid="search-connect-submit"
              >
                {intent.kind === "connect" ? "Connect" : "Save"}
              </Button>
            </div>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  );
}
