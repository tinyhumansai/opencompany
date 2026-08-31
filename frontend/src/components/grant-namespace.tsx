import { useState } from "react";
import { Loader2, ShieldCheck, TriangleAlert } from "lucide-react";
import { toast } from "sonner";

import type { OpenCompanyClient } from "@/api/client";
import { grantTool } from "@/api/tool-grants";
import { Button } from "@/components/ui/button";

/**
 * Grants `namespace` and reports the outcome, returning whether it landed.
 *
 * Separate from the component so a deliberately presentational caller — the
 * provider grid, which owns no host state by design — can offer the same action
 * without acquiring one. Both paths must say the same two things, and the way
 * to guarantee that is for there to be one place that says them.
 */
export async function grantNamespace(
  client: OpenCompanyClient,
  company: string | null,
  namespace: string,
): Promise<boolean> {
  try {
    const grants = await grantTool(client, company, namespace);
    // The host's own sentence about when this bites, not a paraphrase: the
    // grant lands on the company's next turn, and an operator told "done" who
    // then watches the current turn behave exactly as before would reasonably
    // conclude the button did nothing.
    toast.success(`This company now grants ${namespace}`, {
      description: grants.takesEffect,
    });
    return true;
  } catch (err) {
    // Loud on purpose. A grant that silently no-ops is strictly worse than the
    // dead end it replaces: the operator leaves believing it worked.
    toast.error(`Could not grant ${namespace}`, {
      description: err instanceof Error ? err.message : String(err),
    });
    return false;
  }
}

/**
 * The control that ends the #1796 dead end: a connect surface that can add the
 * `[tools].allow` grant it needs, instead of telling an operator it cannot.
 *
 * # The dead end
 *
 * Connecting an integration stores a credential; it does not grant the tool
 * namespace. Those are separate steps, and the console could only ever do the
 * first — so five surfaces (Chargebee, PayPal, hosting, search, Composio) each
 * ended with a variant of *"Add `x` to `[tools].allow` in the company's manifest
 * — it cannot be fixed from this page."* The sentence was accurate, which is
 * what made it a product failure rather than a copy failure: the integration
 * read **Connected** and reached nobody, and on a hosted tenant the manifest is
 * a read-only boot snapshot, so there was no page and no file anywhere the
 * operator could go.
 *
 * # Why the copy still explains, and does not just show a button
 *
 * Because the grant is a real capability decision, not a formality. The
 * catch-all `*` deliberately refuses these namespaces — they reach a real
 * business's customers, wallet, public identity or third-party accounts — so an
 * operator clicking here is widening what their agents can do, and should be
 * told that in one line rather than nudged through it. What changes is that the
 * line now ends in an action instead of an apology.
 *
 * # Why failure is loud
 *
 * A grant that silently no-ops would be strictly worse than the dead end it
 * replaces: the operator would leave believing the integration works. So a
 * refusal (a non-admin, a host that does not offer this namespace) surfaces as a
 * toast carrying the host's own words, and the alert stays put.
 */
export function GrantNamespace({
  client,
  company,
  namespace,
  explanation,
  canManage,
  onGranted,
  testId,
}: {
  client: OpenCompanyClient;
  company: string | null;
  /** The `[tools].allow` namespace to grant, e.g. `chargebee`. */
  namespace: string;
  /** What the company loses without it, in one sentence, in the caller's voice. */
  explanation: string;
  /**
   * Whether this operator may widen the company's grants. `false` renders the
   * explanation with no control — telling someone to click a button that will
   * 403 is the same dead end in a new costume.
   *
   * **Required, and deliberately not defaulted.** It was `= true` for one
   * revision, which is the wrong direction for a permission: a caller that had
   * not yet worked out the viewer's role got an enabled button by omission, and
   * every write behind it is admin-only (`AdminScopedCompany`). Required means a
   * new connect surface cannot acquire this control without first answering the
   * question — the compiler asks on the component's behalf.
   */
  canManage: boolean;
  /** Re-read the surface's own status, so its badge stops saying "not granted". */
  onGranted: () => void | Promise<void>;
  testId?: string;
}) {
  const [busy, setBusy] = useState(false);

  const grant = async () => {
    setBusy(true);
    try {
      if (await grantNamespace(client, company, namespace)) await onGranted();
    } finally {
      setBusy(false);
    }
  };

  return (
    <div
      className="flex flex-col gap-2 rounded-md bg-muted/40 p-2 text-xs text-muted-foreground"
      data-testid={testId}
    >
      <span className="flex items-start gap-2">
        <TriangleAlert className="mt-px size-3 shrink-0" />
        <span>
          {explanation} This company does not grant the{" "}
          <span className="font-mono">{namespace}</span> tool namespace, and a
          catch-all <span className="font-mono">*</span> deliberately does not
          confer it.
          {canManage
            ? " Granting it here widens what this company's teammates can do."
            : " An admin has to grant it before teammates receive the tools."}
        </span>
      </span>
      {canManage ? (
        <div>
          <Button
            size="sm"
            variant="outline"
            disabled={busy}
            onClick={grant}
            data-testid={testId ? `${testId}-action` : undefined}
          >
            {busy ? (
              <Loader2 className="size-4 animate-spin" />
            ) : (
              <ShieldCheck className="size-4" />
            )}
            Grant {namespace}
          </Button>
        </div>
      ) : null}
    </div>
  );
}
