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
}

type Load = "loading" | "ready" | "unavailable";

/**
 * Configure the company's Telegram channel: store the bot token + webhook
 * secret (both write-only), show the webhook URL to paste into BotFather /
 * `setWebhook`, and register the webhook when the host has the transport wired.
 */
export function ChannelsSection({ client, company }: Props) {
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
      toast.error("Enter a bot token and/or a webhook secret to save.");
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
      // A 404 means the host has no outbound Telegram transport in this build.
      toast.error(
        err instanceof ApiError && err.code === "not_wired"
          ? "This host can't call Telegram directly — paste the webhook URL into BotFather instead."
          : err instanceof ApiError
            ? err.message
            : "Couldn't register the webhook.",
      );
    } finally {
      setBusy(null);
    }
  }

  async function copyWebhookUrl() {
    if (!status) return;
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
              Receive and reply to Telegram DMs. Create a bot with @BotFather, paste its token and
              a webhook secret below, then point the bot&apos;s webhook at the URL shown here.
            </p>
          </div>

          {/* The public webhook URL to register with Telegram / BotFather. */}
          {status && (
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
          )}

          <div className="grid gap-3 sm:grid-cols-2">
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
            <div className="space-y-1">
              <Label htmlFor="tg-secret" className="text-xs">
                Webhook secret {status?.secretSet ? "(stored — leave blank to keep)" : ""}
              </Label>
              <Input
                id="tg-secret"
                type="password"
                autoComplete="off"
                placeholder={status?.secretSet ? "•••••• write-only" : "a long random string"}
                value={webhookSecret}
                onChange={(e) => setWebhookSecret(e.target.value)}
              />
            </div>
          </div>

          <div className="flex flex-wrap items-center gap-2">
            <Button disabled={busy !== null} onClick={() => void save()}>
              {busy === "save" ? <Loader2 className="size-4 animate-spin" /> : <Check className="size-4" />}
              Save
            </Button>
            <Button
              variant="outline"
              disabled={busy !== null || !status?.configured}
              onClick={() => void register()}
            >
              {busy === "webhook" ? (
                <Loader2 className="size-4 animate-spin" />
              ) : (
                <Send className="size-4" />
              )}
              Register webhook
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
        </CardContent>
      </Card>
    </section>
  );
}
