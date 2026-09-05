import { useEffect, useState } from "react";

import { me as fetchMe } from "@/api/auth";
import type { OpenCompanyClient } from "@/api/client";
import { ApiError } from "@/api/types";

/**
 * Whether the signed-in viewer may administer this company.
 *
 * Courtesy, never enforcement: every write behind an admin-only control answers
 * `403 only an admin can do that` whatever this returns. What it decides is
 * whether the console *offers* the control at all.
 *
 * # Why this is a hook and not eight copies of an effect
 *
 * It was eight copies of an effect. Ten, counting the surfaces outside Settings.
 * Each one resolved the role correctly — and resolving it was never the part
 * that went wrong. Two pages resolved it and then wired the answer into one
 * sub-component while their own credential form, Save and Disconnect went on
 * rendering enabled for a member; two more never asked at all. A page could be
 * half-gated because "know the role" and "use the role" were separate acts, and
 * nothing connected them.
 *
 * One definition does not by itself close that gap — a caller can still ignore
 * what it returns. What it does is make the gate cheap enough that there is no
 * reason to reach for half of it, and put the question in one place where the
 * answer, and this warning, are read together.
 *
 * # Why it fails closed
 *
 * `false` until the read answers, and `false` if it never does. An unresolved
 * role must not render an enabled button: the cost of being briefly wrong that
 * way is an admin seeing a read-only notice for one round trip, and the cost of
 * being wrong the other way is inviting someone to paste a live credential into
 * a form that can only refuse it.
 *
 * A host with no user plane, or a signed-out console, lands here too — with one
 * exception. `AdminScopedCompany` (`scope.rs`) admits the platform bearer
 * ({@link OpenCompanyClient.carriesPlatformBearer}) directly, with no session
 * behind it to resolve; failing this hook closed against that principal would
 * hide a control the backend has already agreed to run. `resolve_principal`
 * prefers a session over the bearer when both are present, so a session that
 * resolves at all — even to a member — still decides the answer here too.
 *
 * The bearer default only stands in for a read that came back proving no
 * session exists (`/auth/me`'s own 401). A timeout, a 5xx, or anything else
 * that merely failed to answer stays closed — it hasn't shown the backend
 * would refuse a session, only that this particular read didn't get one.
 */
export function useCanManage(client: OpenCompanyClient, company: string | null): boolean {
  return useResolvedManage(client, company, client.carriesPlatformBearer);
}

/**
 * Whether the signed-in viewer may set this company's policy — `PUT …/policy`
 * (`set_policy`, `policy.rs`), which calls `require_admin` straight off the
 * request headers rather than going through `AdminScopedCompany`. That
 * resolves only a human session and refuses a bearer with no session behind
 * it as unauthenticated, unlike the hosting/search/domain/SMTP writes
 * {@link useCanManage} gates, which `AdminScopedCompany` admits the platform
 * bearer for directly. Offering the policy controls to a bearer-only client
 * would invite a request `require_admin` can only 401.
 */
export function useCanManagePolicy(client: OpenCompanyClient, company: string | null): boolean {
  return useResolvedManage(client, company, false);
}

/** Whether `err` is `/auth/me` answering its documented no-session 401, rather than a transport or server failure that merely didn't resolve one. */
function provesNoSession(err: unknown): boolean {
  return err instanceof ApiError && err.status === 401 && err.fromHost;
}

interface Resolved {
  client: OpenCompanyClient;
  company: string | null;
  bearerDefault: boolean;
  manage: boolean;
}

/** Shared resolution: a confirmed human admin session, or `bearerDefault` only when the read proves no session exists. */
function useResolvedManage(
  client: OpenCompanyClient,
  company: string | null,
  bearerDefault: boolean,
): boolean {
  const [resolved, setResolved] = useState<Resolved | null>(null);

  useEffect(() => {
    let live = true;
    void (async () => {
      let manage = bearerDefault;
      try {
        manage = (await fetchMe(client, company)).role === "admin";
      } catch (err) {
        manage = provesNoSession(err) ? bearerDefault : false;
      }
      if (live) setResolved({ client, company, bearerDefault, manage });
    })();
    return () => {
      live = false;
    };
  }, [client, company, bearerDefault]);

  const current =
    resolved !== null &&
    resolved.client === client &&
    resolved.company === company &&
    resolved.bearerDefault === bearerDefault;
  return current ? resolved.manage : false;
}
