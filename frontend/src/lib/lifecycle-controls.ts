import type { UserRole } from "@/api/auth";
import type { LifecycleAction } from "@/api/client";

/**
 * Which lifecycle controls the console may honestly offer (issue #1401).
 *
 * The four buttons in `Settings → General → Lifecycle` do not share an
 * authorization story, and the console used to render them as if they did:
 *
 * - `pause` / `resume` are `CompanyAuth` routes gated by `AdminScopedCompany`:
 *   a person signed in with a magic link reaches them only when their role on
 *   this company is `admin` — an ordinary member is refused with `403`.
 * - `suspend` / `archive` are `PlatformScope` routes. That extractor resolves
 *   through `resolve_claims`, which cannot return a human, so a session cookie
 *   can never reach them *whatever it contains*. The console only ever holds
 *   platform scope when it was handed a platform bearer (`?token=` /
 *   `VITE_OC_TOKEN`), which is not how somebody signing in with a magic link
 *   arrives.
 *
 * So `Archive` — styled `destructive`, behind a dialog calling itself
 * permanent — took the confirmation and then answered `401 unauthorized`. That
 * is the one failure mode this console is otherwise careful about: Billing and
 * Hosting both say, in the page, when a control cannot work here. Lifecycle
 * instead invited an irreversible decision it could not carry out.
 *
 * `platform` here means *this client sends a platform bearer*, not *that bearer
 * carries the scope* — a tenant token without `platform` still gets a `403`.
 * That residue is deliberate: a wrong-scope token is a **configuration**
 * mistake an operator can fix, whereas a session cookie is refused **by
 * construction**, and only the second one is worth hiding a button over.
 *
 * `pause` / `resume` resolve through `resolve_principal`, which prefers a
 * resolved session over a bearer whenever both are present — a console that
 * sends both (`OpenCompanyClient` does) reaches those routes as the signed-in
 * person, not the platform, and a bearer only decides them when no session
 * was found at all. `suspend` / `archive` resolve through `resolve_claims`,
 * which never looks at a session, so `platform` alone still governs them.
 */
export interface LifecycleAffordances {
  /** The actions whose buttons may be rendered, in display order. */
  actions: LifecycleAction[];
  /**
   * Whether to explain that suspend and archive were withheld.
   *
   * Withholding them silently would trade a dishonest button for a missing
   * one, and an operator who read the docs would go looking for the control
   * rather than learning it is not theirs.
   */
  explainPlatformOnly: boolean;
  /**
   * Whether to explain that this company's `suspended` state is not the
   * operator's to lift.
   *
   * `resume` is a `CompanyAuth` route, so the button is reachable — but the
   * handler refuses a non-platform caller specifically when the lifecycle is
   * `suspended`, because that state is a platform-forced pause. Rendering
   * `Resume` there is the same dishonesty as `Archive`, one layer deeper.
   */
  explainPlatformSuspended: boolean;
  /**
   * Whether to explain that pause and resume need admin authority here.
   *
   * A signed-in member reaches the same routes as an admin — `AdminScopedCompany`
   * refuses them with `403`, it does not hide the route — so the console must
   * withhold the button itself rather than let a click end in that toast.
   */
  explainAdminOnly: boolean;
  /** Whether the company is past the end of its lifecycle. */
  archived: boolean;
}

/**
 * @param lifecycle the host's `status.lifecycle` (or the optimistic pending one)
 * @param session the signed-in caller's role, or `null` when the console found no session
 * @param platform whether this client carries a platform bearer
 */
export function lifecycleAffordances(
  lifecycle: string,
  session: UserRole | null,
  platform: boolean,
): LifecycleAffordances {
  const archived = lifecycle === "archived";
  const suspended = lifecycle === "suspended";
  if (archived) {
    return {
      actions: [],
      explainPlatformOnly: false,
      explainPlatformSuspended: false,
      explainAdminOnly: false,
      archived: true,
    };
  }

  // Only decides `pause` / `resume` (and resume's own suspended-lift check),
  // which mirror `resolve_principal`'s session-over-bearer precedence — a
  // bearer counts there only once a session has been ruled out.
  const effectivePlatform = platform && session === null;
  const authorized = session === "admin" || effectivePlatform;
  const actions: LifecycleAction[] = [];
  if (authorized) {
    if (lifecycle === "running") actions.push("pause");
    // A paused company is any admin's to restart; a suspended one is the platform's.
    if (lifecycle === "paused" || (suspended && effectivePlatform)) actions.push("resume");
  }
  // `suspend` / `archive` never look at a session, so the raw bearer decides.
  if (platform) actions.push("suspend", "archive");

  return {
    actions,
    explainPlatformOnly: authorized && !platform,
    explainPlatformSuspended: authorized && suspended && !effectivePlatform,
    explainAdminOnly: !authorized,
    archived: false,
  };
}
