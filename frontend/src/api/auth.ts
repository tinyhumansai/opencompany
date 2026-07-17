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

// ---------------------------------------------------------------------------
// Admin
// ---------------------------------------------------------------------------
//
// Every call below needs an admin session; the backend answers 403 otherwise.
// The UI gates on `me().role` as well, so a member never sees controls that
// would only fail — but the gate that matters is the server's.

/** Whether a user may currently sign in. */
export type UserStatus = "active" | "suspended";

/** A person, as an admin sees them. Never carries a password hash. */
export interface Person {
  id: string;
  email: string;
  displayName?: string;
  role: UserRole;
  status: UserStatus;
  /** Whether they have a password — never what it is. */
  hasPassword: boolean;
  mustChangePassword: boolean;
  createdAtMillis: number;
  lastSeenAtMillis?: number;
}

/**
 * An outstanding invite.
 *
 * An id prefixed `manifest:` is synthetic — that address is an admin because
 * the company manifest says so. It has no stored record, so it cannot be
 * revoked here; edit `[users].admins` instead.
 */
export interface Invite {
  id: string;
  email: string;
  role: UserRole;
  invitedBy: string;
  createdAtMillis: number;
  expiresAtMillis: number;
  acceptedAtMillis?: number;
}

/** Whether an invite comes from the manifest rather than a stored record. */
export function isManifestInvite(invite: Invite): boolean {
  return invite.id.startsWith("manifest:");
}

/** The company's people. */
export async function listPeople(
  client: OpenCompanyClient,
  company: string | null,
): Promise<Person[]> {
  return client.get<Person[]>(`${client.scopeFor(company)}/users`);
}

/** Outstanding invites, including the manifest's standing admins. */
export async function listInvites(
  client: OpenCompanyClient,
  company: string | null,
): Promise<Invite[]> {
  return client.get<Invite[]>(`${client.scopeFor(company)}/users/invites`);
}

/** Invites an address. */
export async function invite(
  client: OpenCompanyClient,
  company: string | null,
  email: string,
  role: UserRole,
): Promise<Invite> {
  return client.post<Invite>(`${client.scopeFor(company)}/users/invites`, { email, role });
}

/** Revokes an invite. */
export async function revokeInvite(
  client: OpenCompanyClient,
  company: string | null,
  inviteId: string,
): Promise<void> {
  await client.del(`${client.scopeFor(company)}/users/invites/${encodeURIComponent(inviteId)}`);
}

/** Changes a person's role, status, or display name. */
export async function updatePerson(
  client: OpenCompanyClient,
  company: string | null,
  userId: string,
  changes: { role?: UserRole; status?: UserStatus; displayName?: string },
): Promise<Person> {
  return client.patch<Person>(
    `${client.scopeFor(company)}/users/${encodeURIComponent(userId)}`,
    changes,
  );
}

/**
 * Sets a temporary password for someone.
 *
 * Revokes their sessions and flags the account, so the next thing they can do
 * is replace it. The admin must convey the value out of band — which is the
 * cost of this option, and why the magic link is usually the better answer.
 */
export async function resetPassword(
  client: OpenCompanyClient,
  company: string | null,
  userId: string,
  password: string,
): Promise<Person> {
  return client.post<Person>(
    `${client.scopeFor(company)}/users/${encodeURIComponent(userId)}/password`,
    { password },
  );
}

/** Signs someone out everywhere. */
export async function revokeSessions(
  client: OpenCompanyClient,
  company: string | null,
  userId: string,
): Promise<{ revoked: number }> {
  return client.del<{ revoked: number }>(
    `${client.scopeFor(company)}/users/${encodeURIComponent(userId)}/sessions`,
  );
}
