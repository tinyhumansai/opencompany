// The Telegram channel API (issue #31): the console reads and writes the
// company's Telegram configuration through the host's `.../channels/telegram`
// routes (REST, camelCase over the wire).
//
// The bot token and webhook secret are write-only: they are sent on PUT and
// stored in the host's secret store; neither is ever returned. The read shape
// (`TelegramChannelStatus`) carries only presence booleans and the public
// webhook URL. Mirrors `api/mcp.ts` — standalone functions over the shared
// client, so no change to `OpenCompanyClient` is needed.

import type { OpenCompanyClient } from "./client";

/** The non-secret status of a company's Telegram channel. */
export interface TelegramChannelStatus {
  /** True once both the bot token and the webhook secret are stored. */
  configured: boolean;
  /** Whether a bot token is stored (never the token itself). */
  tokenSet: boolean;
  /** Whether a webhook secret is stored (never the secret itself). */
  secretSet: boolean;
  /** The URL to register with Telegram (`setWebhook`) / paste into BotFather. */
  webhookUrl: string;
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
  webhookUrl: string;
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
