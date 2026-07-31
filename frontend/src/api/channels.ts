// The Telegram channel API (issue #31): the console reads and writes the
// company's Telegram configuration through the host's `.../channels/telegram`
// routes (REST, camelCase over the wire).
//
// The bot token and webhook secret are write-only: they are sent on PUT and
// stored in the host's secret store; neither is ever returned. The read shape
// (`TelegramChannelStatus`) carries only presence booleans, how inbound
// arrives, and the webhook URL when the host has a usable one. Mirrors
// `api/mcp.ts` — standalone functions over the shared client, so no change to
// `OpenCompanyClient` is needed.
//
// Issue #203: a bot token alone is a working channel. Inbound arrives over
// `getUpdates` long-polling, which dials out and so needs no public URL; the
// webhook is an optional hosted fast-path the host advertises (a non-null
// `webhookUrl`) only when Telegram could actually deliver to it.

import type { OpenCompanyClient } from "./client";

/** The non-secret status of a company's Telegram channel. */
export interface TelegramChannelStatus {
  /** True once a bot token is stored — that alone is a working channel. */
  configured: boolean;
  /** Whether a bot token is stored (never the token itself). */
  tokenSet: boolean;
  /** Whether a webhook secret is stored (never the secret itself). */
  secretSet: boolean;
  /**
   * The URL to register with Telegram (`setWebhook`), or `null` when this host
   * has no publicly reachable https URL — every local and most self-hosted
   * deployment. Never show a webhook affordance while this is `null`.
   */
  webhookUrl: string | null;
  /** Whether this host long-polls Telegram for inbound updates. */
  polling: boolean;
}

/** The write-only config body. Only present, non-empty fields are applied. */
export interface TelegramConfigBody {
  /** Bot token from BotFather. Omit to leave the stored token unchanged. */
  botToken?: string;
  /** Webhook secret token. Omit to leave the stored secret unchanged. */
  webhookSecret?: string;
}

/** The `setWebhook` outcome (carries the non-secret URL, never a credential). */
export interface SetWebhookResult {
  ok: boolean;
  message: string;
  webhookUrl: string | null;
}

/** Read the company's Telegram channel status. */
export function getTelegramChannel(
  client: OpenCompanyClient,
  company: string | null,
): Promise<TelegramChannelStatus> {
  return client.get<TelegramChannelStatus>(`${client.scopeFor(company)}/channels/telegram`);
}

/** Store the bot token and/or webhook secret (write-only). */
export function putTelegramChannel(
  client: OpenCompanyClient,
  company: string | null,
  body: TelegramConfigBody,
): Promise<TelegramChannelStatus> {
  return client.put<TelegramChannelStatus>(
    `${client.scopeFor(company)}/channels/telegram`,
    body,
  );
}

/** Clear both stored credentials. */
export function deleteTelegramChannel(
  client: OpenCompanyClient,
  company: string | null,
): Promise<TelegramChannelStatus> {
  return client.del<TelegramChannelStatus>(`${client.scopeFor(company)}/channels/telegram`);
}

/** Register the webhook with Telegram (host must have the transport wired). */
export function setTelegramWebhook(
  client: OpenCompanyClient,
  company: string | null,
): Promise<SetWebhookResult> {
  return client.post<SetWebhookResult>(
    `${client.scopeFor(company)}/channels/telegram/webhook`,
  );
}
