// The user-authentication surface: magic link, password, session.
//
// For a console served by the host it talks to — every same-origin deployment,
// which is the normal one — the session is an HttpOnly cookie: none of this
// returns or stores a token, the browser holds it, and `credentials: "include"`
// in the client sends it. There is nothing here for an XSS to read.
//
// A console on a *different* origin from its host gets no cookie at all: the
// host sets it `SameSite=Lax` and the browser withholds it from every
// cross-site request. Those sign-ins ask for a token instead and return it as
// `SignIn.session`, which the caller stores on the connection. See
// `Credential` in `connections/types.ts` for what that costs and why it is
// still the right trade where the alternative is no console at all.

import type { OpenCompanyClient } from "./client";

/** What a company may call a user. */
export type UserRole = "admin" | "member";

/**
 * How a company signs people in.
 *
 * - `email` — magic link, optional password, ecosystem buttons. The default,
 *   and what every company did before this was configurable.
 * - `wallet` — a signed challenge from an Ed25519 (Solana-style) wallet. No
 *   mailbox is involved anywhere, so nothing is emailed and no password exists.
 * - `none` — there is no sign-in. The app on this device is the owner, and the
 *   console never renders a login screen at all.
 */
export type AuthMode = "email" | "wallet" | "none";

/** What the console must know before it can draw a sign-in screen. */
export interface AuthConfig {
  mode: AuthMode;
  /**
   * What this company calls itself, so the sign-in screen can name what a
   * credential is being handed to.
   *
   * It has to arrive here rather than from `status`: every route that reports
   * the name is behind the very sign-in being drawn, and on the hosted platform
   * each tenant is a separate company on its own URL, so "which one is this"
   * is a real question at that moment.
   *
   * Optional only for a host predating the field. The console draws a heading
   * with no name there — the same one it drew before this existed.
   */
  name?: string;
  /** Whether a password may be offered. Only ever true in `email` mode. */
  passwords: boolean;
  /**
   * Whether a magic link asked for here reaches anybody: the host has a mail
   * transport, or it is loopback-bound and hands the code straight back.
   *
   * False is a routable host with no transport, and the console has to act on
   * it — `auth/request` answers `sent: true` there exactly as it does on a host
   * that delivered, so nothing else in the flow will ever reveal that the link
   * went nowhere.
   */
  magicLink: boolean;
}

/**
 * Asks the host how this company signs people in.
 *
 * Unauthenticated, and it has to be: the console asks before anyone has a
 * credential, because it cannot choose a screen otherwise. Branch on this rather
 * than on which routes fail — a company with no sign-in must render "open the
 * desktop app", not an email box that can never work.
 *
 * Defaults to `email` if the host cannot answer, which is what every host
 * predating this route does.
 *
 * `magicLink` defaults to true through both kinds of rollout skew — no route,
 * and a route that omits the field — because a host old enough not to report it
 * either mails links or echoes them. Assuming false there would withdraw a
 * working sign-in from every deployment that has not updated yet.
 */
export async function fetchAuthConfig(
  client: OpenCompanyClient,
  company: string | null,
): Promise<AuthConfig> {
  try {
    const config = await client.get<AuthConfig>(`${client.scopeFor(company)}/auth/config`);
    // A blank name is not a name. Normalised here, once, so no view has to
    // decide whether `""` means "unnamed" or "not reported".
    return {
      ...config,
      name: config.name?.trim() || undefined,
      magicLink: config.magicLink ?? true,
    };
  } catch {
    return { mode: "email", passwords: true, magicLink: true };
  }
}

/** The signed-in user, as `GET .../auth/me` reports them. */
export interface Me {
  id: string;
  email: string;
  displayName?: string;
  /**
   * The face they chose (`lib/avatar.ts`), absent when they have not chosen —
   * which the console draws as the mascot it hashes from their id.
   */
  avatar?: string;
  role: UserRole;
  company: string;
  /** Whether they have a password, never what it is. */
  hasPassword: boolean;
  /** An admin issued a temporary password that should be replaced. */
  mustChangePassword: boolean;
}

/**
 * What a successful sign-in returns.
 *
 * `session` is present only when the client asked the host to mint a session it
 * would carry itself — a console on a different origin from its host, where no
 * cookie can work. See `Credential` in `connections/types.ts`.
 *
 * A caller that receives one **must store it** (`adoptSession`), or the sign-in
 * appears to succeed and the very next request is anonymous: the token comes
 * back exactly once and only its hash is kept server-side.
 */
export type SignIn = Me & { session?: string };

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
  redirect?: string,
): Promise<RequestCodeResult> {
  return client.post<RequestCodeResult>(`${client.scopeFor(company)}/auth/request`, {
    email,
    // The landing fragment a *mailed* link should carry (setup's hand-off
    // passes `#/company?from=setup`). Absent for a normal sign-in, which lands
    // wherever it always did. `undefined` is dropped by JSON.stringify, so the
    // body is unchanged unless a redirect was actually asked for.
    redirect,
  });
}

/** Redeems a magic link for a session. */
export async function verifyCode(
  client: OpenCompanyClient,
  company: string | null,
  code: string,
): Promise<SignIn> {
  return client.postSignIn<SignIn>(`${client.scopeFor(company)}/auth/verify`, { code });
}

/** One ecosystem sign-in button, as the host describes it. */
export interface HubProvider {
  /** The hub's provider slug (`google`, `github`, `twitter`). */
  id: string;
  /** What to put on the button. */
  label: string;
  /**
   * Where to send the browser. Built by the host, never assembled here: only
   * the host knows the hub's base URL and the origin the hub must return to,
   * and a console guessing at either would aim a live sign-in at its guess.
   */
  startUrl: string;
}

/**
 * Asks the host which ecosystem providers it can sign someone in with.
 *
 * An empty list is the normal answer on a self-hosted host and is not an
 * error — it means "no ecosystem here, show the magic-link form alone". So this
 * never throws for that case; callers only need to handle the network failing.
 *
 * `from`, when present, is the destination the host should put on the sign-in's
 * return URI — the console's own fragment cannot cross the OAuth round trip, so
 * the host carries it as a query parameter the landing reads back. Only setup's
 * dead-link recovery asks for one today (`from=setup`).
 */
export async function fetchHubProviders(
  client: OpenCompanyClient,
  company: string | null,
  from?: string,
): Promise<HubProvider[]> {
  const result = await client.get<{ providers: HubProvider[] }>(
    `${client.scopeFor(company)}/auth/hub${from ? `?from=${encodeURIComponent(from)}` : ""}`,
  );
  return result.providers ?? [];
}

/**
 * Turns a platform token from the hub into a session on this company.
 *
 * The token arrives in the URL as `?token=…&key=auth` after the hub completes
 * OAuth and redirects back here. It is not an identity this console can read or
 * check — it is handed straight to the host, which asks the hub whose it is and
 * then applies this company's own roster. So this returns the same result a
 * magic link would, and the hub's token is spent here and stripped from the URL
 * rather than kept — whichever carrier the session itself comes back in.
 *
 * The distinguishable failures are `hub_rejected` (expired or forged — sign in
 * again), `not_a_member` (a real ecosystem account with no access here), and
 * `hub_unavailable` (this host has no ecosystem at all). Read them off
 * {@link ApiError.code}.
 */
export async function signInWithHubToken(
  client: OpenCompanyClient,
  company: string | null,
  token: string,
): Promise<SignIn> {
  return client.postSignIn<SignIn>(`${client.scopeFor(company)}/auth/hub`, { token });
}

/** Exchanges an email and password for a session. */
export async function loginWithPassword(
  client: OpenCompanyClient,
  company: string | null,
  email: string,
  password: string,
): Promise<SignIn> {
  return client.postSignIn<SignIn>(`${client.scopeFor(company)}/auth/login`, { email, password });
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

// ---------------------------------------------------------------------------
// Wallet sign-in
// ---------------------------------------------------------------------------

/** The challenge a wallet must sign. */
export interface WalletChallenge {
  /** Echoed back on verify so the host can find the record. */
  nonce: string;
  /**
   * The exact text to sign, UTF-8.
   *
   * Sign it verbatim. Do not rebuild it here — the layout is versioned by its
   * first line and belongs to the host, and a console that reassembled it would
   * make every future change to that layout a breaking change for this file.
   */
  message: string;
  expiresAtMillis: number;
}

/**
 * Asks for a challenge for `address`.
 *
 * Always succeeds, for every well-formed address, including ones this company
 * has never heard of — the same rule that makes `requestCode` always report
 * `sent`. A challenge is not evidence the wallet may sign in; only
 * {@link verifyWalletSignature} answers that.
 */
export async function requestWalletChallenge(
  client: OpenCompanyClient,
  company: string | null,
  address: string,
): Promise<WalletChallenge> {
  return client.post<WalletChallenge>(`${client.scopeFor(company)}/auth/wallet/challenge`, {
    address,
  });
}

/** Answers a challenge with the wallet's signature, and receives a session. */
export async function verifyWalletSignature(
  client: OpenCompanyClient,
  company: string | null,
  nonce: string,
  signature: string,
): Promise<Me> {
  return client.postSignIn<SignIn>(`${client.scopeFor(company)}/auth/wallet/verify`, {
    nonce,
    signature,
  });
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
  /**
   * The face they chose, absent when they have not chosen.
   *
   * Readable by anyone who can read the roster, and writable **only by the
   * person wearing it** — `updateMe`, not `updatePerson`. An admin may set
   * somebody's `displayName` so a roster of raw addresses can be made legible;
   * a person's own face is theirs to pick.
   */
  avatar?: string;
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
  /**
   * When the invite email was accepted by the transport, if it was sent.
   *
   * Absent means no mail reached anyone — the host has no transport, the send
   * failed, or the invite predates invite mail. The roster says so rather than
   * implying delivery, because "invited" and "told they were invited" are
   * different facts and only the operator can close the gap.
   */
  notifiedAtMillis?: number;
}

/**
 * What happened to the invite email.
 *
 * Reported by the server rather than assumed by the console: an invite lands
 * on a host with no mail transport just as readily as on one with, and the
 * difference decides whether the operator still has to go and tell the person.
 */
export type InviteDelivery = "sent" | "no_transport" | "failed" | "no_mailbox";

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

/**
 * Invites an address, and reports whether they were actually emailed.
 *
 * A 2xx here means the grant landed — it does **not** mean anyone was told.
 * Callers must branch on `delivery` rather than treating success as delivery,
 * which is the bug in issue #584.
 */
export async function invite(
  client: OpenCompanyClient,
  company: string | null,
  identifier: string,
  role: UserRole,
  mode: AuthMode,
): Promise<Invite & { delivery: InviteDelivery }> {
  // Which field carries the identifier follows the company's mode, because the
  // server normalizes them by different rules: an address is lowercased, a
  // base58 key must not be. Sending both, or the wrong one, is refused.
  const body =
    mode === "wallet" ? { wallet: identifier, role } : { email: identifier, role };
  return client.post<Invite & { delivery: InviteDelivery }>(
    `${client.scopeFor(company)}/users/invites`,
    body,
  );
}

/** Revokes an invite. */
export async function revokeInvite(
  client: OpenCompanyClient,
  company: string | null,
  inviteId: string,
): Promise<void> {
  await client.del(`${client.scopeFor(company)}/users/invites/${encodeURIComponent(inviteId)}`);
}

/**
 * Changes your own name or face.
 *
 * Deliberately not `updatePerson`: that one is admin-only and takes a user id,
 * which is right for administering somebody else and wrong for naming yourself.
 * Both fields are three-state — omitted leaves it alone, `null` goes back to the
 * default, a value sets it — so saving a name cannot wipe a face.
 */
export async function updateMe(
  client: OpenCompanyClient,
  company: string | null,
  changes: { displayName?: string | null; avatar?: string | null },
): Promise<Me> {
  return client.patch<Me>(`${client.scopeFor(company)}/auth/me`, changes);
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
