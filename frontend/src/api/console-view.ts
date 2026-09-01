import type { OpenCompanyClient } from "@/api/client";
import type { View } from "@/lib/console-routes";

/**
 * Tell the host which page the operator opened (issue #1739).
 *
 * The console is a single-page app, so moving between pages is a hash change
 * and no request reaches the host — "which surfaces do operators actually use"
 * is otherwise a question the product cannot answer about itself.
 *
 * **The view, never the hash.** `#/chat/dm:ada-1f3k` names a teammate and
 * `#/tasks/<uuid>` names a task; only the first segment is a fact about the
 * product rather than about the company using it. `View` is the routed-view
 * union, so this cannot be handed a hash by accident, and the host folds it
 * onto its own closed list again on arrival — neither side trusts the other.
 *
 * Fire-and-forget by construction. A rejected promise is swallowed: an
 * operator navigating the console must never see a toast, a retry, or a
 * blocked render because a telemetry write failed, and a host with analytics
 * off answers `204` from a null tracker anyway.
 */
export function reportConsoleView(
  client: OpenCompanyClient,
  company: string | null,
  view: View,
): void {
  void client
    .post(`${client.scopeFor(company)}/analytics/console-view`, { view })
    .catch(() => {});
}
