import { useCallback, useEffect, useState } from "react";
import { Check, Copy, Loader2, Send, Trash2 } from "lucide-react";
import { toast } from "sonner";

import type { OpenCompanyClient } from "@/api/client";
import {
  deleteTelegramChannel,
  getTelegramChannel,
  putTelegramChannel,
  setTelegramWebhook,
  type TelegramChannelStatus,
} from "@/api/channels";
import { ApiError } from "@/api/types";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";

interface Props {
  client: OpenCompanyClient;
  company: string | null;
  /**
   * Whether this viewer may change the bot the company speaks as (issue #403).
   * Courtesy only — the host answers 403 regardless.
   */
  canManage: boolean;
}

type Load = "loading" | "ready" | "unavailable";

/**
 * Configure the company's Telegram channel.
 *
 * Setup is a bot token, and nothing else: the host receives inbound over
 * `getUpdates` long-polling, which dials out to Telegram and therefore works on
 * localhost, behind NAT, and on any self-hosted box (issue #203).
 *
 * The inbound webhook is an optional hosted fast-path. The host advertises it
 * by returning a non-null `webhookUrl`, which it only does when it has a
 * publicly reachable https URL — so this card shows the webhook block *only*
 * then, rather than handing the operator a `http://127.0.0.1:<port>/hooks/...`
 * URL that Telegram's servers can never deliver to.
 */
export function ChannelsSection({ client, company, canManage }: Props) {
  const [load, setLoad] = useState<Load>("loading");
  const [status, setStatus] = useState<TelegramChannelStatus | null>(null);
  const [botToken, setBotToken] = useState("");
  const [webhookSecret, setWebhookSecret] = useState("");
  const [busy, setBusy] = useState<string | null>(null);

  const refresh = useCallback(async () => {
    try {
      setStatus(await getTelegramChannel(client, company));
      setLoad("ready");
    } catch {
      // The channels surface isn't wired on this host — hide the section.
      setLoad("unavailable");
    }
  }, [client, company]);

  useEffect(() => {
    setLoad("loading");
    void refresh();
  }, [refresh]);

  async function save() {
    if (busy) return;
    if (!botToken.trim() && !webhookSecret.trim()) {
      toast.error("Enter a bot token to save.");
      return;
    }
    setBusy("save");
    try {
      const next = await putTelegramChannel(client, company, {
        botToken: botToken.trim() || undefined,
        webhookSecret: webhookSecret.trim() || undefined,
      });
      setStatus(next);
      setBotToken("");
      setWebhookSecret("");
      toast.success("Telegram credentials saved.");
    } catch (err) {
      toast.error(err instanceof ApiError ? err.message : "Couldn't save the Telegram config.");
    } finally {
      setBusy(null);
    }
  }

  async function clear() {
    if (busy) return;
    setBusy("clear");
    try {
      setStatus(await deleteTelegramChannel(client, company));
      toast.success("Telegram credentials cleared.");
    } catch (err) {
      toast.error(err instanceof ApiError ? err.message : "Couldn't clear the Telegram config.");
    } finally {
      setBusy(null);
    }
  }

  async function register() {
    if (busy) return;
    setBusy("webhook");
    try {
      const res = await setTelegramWebhook(client, company);
      if (res.ok) {
        toast.success("Webhook registered with Telegram.");
      } else {
        toast.error(res.message);
      }
    } catch (err) {
      // A 404 means the host has no outbound Telegram transport in this build,
      // in which case neither the webhook nor polling can work.
      toast.error(
        err instanceof ApiError && err.code === "not_wired"
          ? "This host can't reach Telegram — rebuild it with the `telegram` feature."
          : err instanceof ApiError
            ? err.message
            : "Couldn't register the webhook.",
      );
    } finally {
      setBusy(null);
    }
  }

  async function copyWebhookUrl() {
    if (!status?.webhookUrl) return;
    try {
      await navigator.clipboard.writeText(status.webhookUrl);
      toast.success("Webhook URL copied.");
    } catch {
      toast.error("Couldn't copy — select and copy the URL manually.");
    }
  }

  // Hidden entirely until we know the surface exists (keeps the page quiet on
  // hosts without the channels routes).
  if (load === "unavailable") return null;

  return (
    <section className="space-y-3">
      <div className="flex items-center justify-between gap-3">
        <h3 className="text-xs font-medium tracking-wide text-muted-foreground uppercase">
          Channels
        </h3>
        {load === "ready" && status?.configured && (
          <Badge variant="secondary" className="gap-1">
            <Check className="size-3" /> Telegram configured
          </Badge>
        )}
      </div>

      <Card>
        <CardContent className="space-y-4 pt-6">
          <div className="space-y-1">
            <p className="text-sm font-medium">Telegram</p>
            <p className="text-sm text-muted-foreground">
              Receive and reply to Telegram DMs. Create a bot with @BotFather and paste its token
              below — that&apos;s the whole setup. This host connects out to Telegram to collect
              messages, so it needs no public URL and no webhook.
            </p>
          </div>

          {!canManage && (
            <p className="rounded-md bg-muted/40 p-2 text-xs text-muted-foreground">
              This bot token is the identity the company speaks as, so an admin manages it. The
              status above is the whole picture; ask an admin to change it.
            </p>
          )}

          {/* The webhook secret sits beside the token only on a host that can
              actually serve a webhook — elsewhere it configures nothing. */}
          {canManage && (
            <div className={status?.webhookUrl ? "grid gap-3 sm:grid-cols-2" : "space-y-1"}>
              <div className="space-y-1">
                <Label htmlFor="tg-token" className="text-xs">
                  Bot token {status?.tokenSet ? "(stored — leave blank to keep)" : ""}
                </Label>
                <Input
                  id="tg-token"
                  type="password"
                  autoComplete="off"
                  placeholder={status?.tokenSet ? "•••••• write-only" : "123456:ABC-DEF…"}
                  value={botToken}
                  onChange={(e) => setBotToken(e.target.value)}
                />
              </div>
              {status?.webhookUrl && (
                <div className="space-y-1">
                  <Label htmlFor="tg-secret" className="text-xs">
                    Webhook secret {status.secretSet ? "(stored — leave blank to keep)" : ""}
                  </Label>
                  <Input
                    id="tg-secret"
                    type="password"
                    autoComplete="off"
                    placeholder={status.secretSet ? "•••••• write-only" : "a long random string"}
                    value={webhookSecret}
                    onChange={(e) => setWebhookSecret(e.target.value)}
                  />
                </div>
              )}
            </div>
          )}

          {/* How inbound actually arrives, so the operator can tell whether a
              saved token is being used. */}
          {status?.configured && status.polling && !status.webhookUrl && (
            <p className="text-xs text-muted-foreground">
              Listening for messages by polling Telegram — no public URL needed.
            </p>
          )}
          {status?.configured && !status.polling && (
            <p className="text-xs text-muted-foreground">
              This host has no outbound Telegram transport, so it can neither collect messages nor
              reply. Rebuild it with the <code className="font-mono">telegram</code> feature.
            </p>
          )}

          {canManage && (
            <div className="flex flex-wrap items-center gap-2">
              <Button disabled={busy !== null} onClick={() => void save()}>
                {busy === "save" ? <Loader2 className="size-4 animate-spin" /> : <Check className="size-4" />}
                Save
              </Button>
              <Button
                variant="ghost"
                disabled={busy !== null || !(status?.tokenSet || status?.secretSet)}
                onClick={() => void clear()}
              >
                {busy === "clear" ? (
                  <Loader2 className="size-4 animate-spin" />
                ) : (
                  <Trash2 className="size-4" />
                )}
                Clear
              </Button>
            </div>
          )}

          {/* The webhook fast-path. Shown only when the host reports a URL
              Telegram can actually deliver to — otherwise offering it would
              hand the operator a dead loopback endpoint (issue #203), and
              registering it would also block the polling that does work. */}
          {status?.webhookUrl && canManage && (
            <div className="space-y-3 border-t pt-4">
              <div className="space-y-1">
                <p className="text-xs font-medium">Webhook (optional)</p>
                <p className="text-xs text-muted-foreground">
                  This host is publicly reachable, so Telegram can push updates straight to it
                  instead of being polled. Save a webhook secret above, then register — polling
                  stands down while a webhook is active.
                </p>
              </div>

              <div className="space-y-1">
                <Label className="text-xs">Webhook URL</Label>
                <div className="flex items-center gap-2">
                  <Input readOnly value={status.webhookUrl} className="font-mono text-xs" />
                  <Button
                    type="button"
                    variant="outline"
                    size="icon"
                    onClick={() => void copyWebhookUrl()}
                    aria-label="Copy webhook URL"
                  >
                    <Copy className="size-4" />
                  </Button>
                </div>
              </div>

              <Button
                variant="outline"
                disabled={busy !== null || !status.configured || !status.secretSet}
                onClick={() => void register()}
              >
                {busy === "webhook" ? (
                  <Loader2 className="size-4 animate-spin" />
                ) : (
                  <Send className="size-4" />
                )}
                Register webhook
              </Button>
            </div>
          )}
        </CardContent>
      </Card>
    </section>
  );
}
