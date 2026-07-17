// The user-authentication surface: magic link, password, session.
//
// The session itself is an HttpOnly cookie, so none of this returns or stores a
// token — the browser holds it and `credentials: "include"` in the client sends
// it. There is nothing here for an XSS to read.

import type { OpenCompanyClient } from "./client";

/** What a company may call a user. */
export type UserRole = "admin" | "member";

/** The signed-in user, as `GET .../auth/me` reports them. */
export interface Me {
  id: string;
  email: string;
  displayName?: string;
  role: UserRole;
  company: string;
  /** Whether they have a password, never what it is. */
  hasPassword: boolean;
  /** An admin issued a temporary password that should be replaced. */
  mustChangePassword: boolean;
}

/**
 * The answer to "send me a link".
 *
 * `sent` is always true, for everyone, including addresses with no account —
 * the backend refuses to say, because telling apart "no such user" from "wrong
 * secret" would let anyone enumerate the company's membership. Do not surface
 * anything but "check your mail".
 */
export interface RequestCodeResult {
  sent: boolean;
  /**
   * The login code, present only on a host with no mail transport configured
   * (local development). Never present anywhere that can actually send mail.
   */
  dev_code?: string;
}

/** Asks for a magic link. */
export async function requestCode(
  client: OpenCompanyClient,
  company: string | null,
  email: string,
): Promise<RequestCodeResult> {
  return client.post<RequestCodeResult>(`${client.scopeFor(company)}/auth/request`, { email });
}

/** Redeems a magic link for a session. */
export async function verifyCode(
  client: OpenCompanyClient,
  company: string | null,
  code: string,
): Promise<Me> {
  return client.post<Me>(`${client.scopeFor(company)}/auth/verify`, { code });
}

/** Exchanges an email and password for a session. */
export async function loginWithPassword(
  client: OpenCompanyClient,
  company: string | null,
  email: string,
  password: string,
): Promise<Me> {
  return client.post<Me>(`${client.scopeFor(company)}/auth/login`, { email, password });
}

/** Who the current session belongs to; throws 401 when signed out. */
export async function me(client: OpenCompanyClient, company: string | null): Promise<Me> {
  return client.get<Me>(`${client.scopeFor(company)}/auth/me`);
}

/** Sets or replaces the signed-in user's own password. */
export async function setPassword(
  client: OpenCompanyClient,
  company: string | null,
  password: string,
): Promise<Me> {
  return client.post<Me>(`${client.scopeFor(company)}/auth/password`, { password });
}

/** Revokes this session, server-side and in the browser. */
export async function logout(client: OpenCompanyClient, company: string | null): Promise<void> {
  await client.post(`${client.scopeFor(company)}/auth/logout`, {});
}
