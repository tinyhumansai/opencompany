import { describe, expect, it } from "vitest";

import type { ComposioStatus } from "@/api/composio";
import { composioForm, composioRows } from "@/composio/rows";
import type { ComposioPending } from "@/composio/types";

/**
 * The invariant `showManagedTokenCard` used to hold, now held by the shape.
 *
 * # What this file used to test, and why it changed
 *
 * `showManagedTokenCard(mode, onByok, canManage, credentialed, showOverride,
 * byoToken)` gated the legacy managed-route token card, and it required the
 * SELECTED tile and the PERSISTED route to *both* say managed. Either alone put
 * two credential surfaces on screen at exactly the moment an operator was
 * switching between routes, and the two directions broke differently:
 *
 * - **Selected-only** showed the card to a BYOK company that had merely clicked
 *   the managed tile. Its Clear calls the legacy `setComposioToken("")`, which
 *   erases the *preserved* backend-token override (issue #586) without touching
 *   `composio/api_key` or `composio/mode` at all — a button that looks like the
 *   way back to managed, silently destroying a token the design keeps
 *   specifically in order to restore, and leaving the company on BYOK anyway.
 * - **Persisted-only** showed it to a managed company that had clicked the BYOK
 *   tile, so a Composio API key field and a legacy token field were both live
 *   with no way to tell which one the Save below belonged to.
 *
 * The tiles are gone; the failures are not hypothetical and the invariant still
 * has to hold. So these tests were **retargeted, not deleted**: the six-input
 * predicate is replaced by `composioForm`, which returns **at most one**
 * `ComposioForm` by type, and by `composioRows`, which decides which row may
 * offer a credential control at all. Two credential surfaces are now
 * unrepresentable rather than merely tested against, and what is left to check
 * is that no route reaches the wrong row's form.
 */

function status(over: Partial<ComposioStatus> = {}): ComposioStatus {
  return {
    inBuild: true,
    granted: true,
    credentialSource: "attested",
    mode: "managed",
    backendUrl: "https://backend.composio.dev",
    toolkits: [],
    openMode: true,
    effectiveToolkits: [],
    effectiveCatalog: [],
    catalogSource: "manifest",
    catalogNotice: null,
    ...over,
  };
}

const formFor = (pending: ComposioPending, over: Partial<ComposioStatus> = {}) =>
  composioForm(pending, composioRows(status(over)));

describe("exactly one credential surface, in either direction of the switch", () => {
  it("refuses the managed token form to a company persisted on BYOK", () => {
    // The selected-only failure, at its source: there is no longer a "selected
    // tile" to disagree with the stored route, and the managed row offers no
    // credential control while it is not the active one — so the form that used
    // to destroy a preserved override cannot be reached at all.
    const byok = { mode: "byok", credentialSource: "static" } as const;

    expect(formFor({ row: "managed", action: "add" }, byok)).toBeNull();
    expect(formFor({ row: "managed", action: "replace" }, byok)).toBeNull();
  });

  it("refuses it even when the managed chain itself holds a token", () => {
    // `managedCredentialSource: "static"` is exactly the preserved override the
    // old Clear destroyed. It is legible on the row — and still not editable
    // from a company that is not on that route.
    expect(
      formFor(
        { row: "managed", action: "replace" },
        { mode: "byok", credentialSource: "static", managedCredentialSource: "static" },
      ),
    ).toBeNull();
  });

  it("refuses the API key form to a company persisted on managed — except as the way in", () => {
    // The persisted-only failure. A managed company reaching the own-account
    // row gets exactly one field, and it is the API key: `add` is the hand-off
    // that `Use this` performs, and `replace` has nothing to rotate.
    const managed = { mode: "managed", credentialSource: "attested" } as const;

    expect(formFor({ row: "byok", action: "replace" }, managed)).toBeNull();
    expect(formFor({ row: "byok", action: "add" }, managed)).toMatchObject({
      row: "byok",
      credential: "composio-api-key",
    });
  });

  it("puts each row's form on the route that row actually writes", () => {
    // The two credentials authenticate different hosts — `composio/token` is a
    // bearer the TinyHumans backend recognises, `composio/api-key` is a key
    // Composio itself recognises — so a form pointed at the wrong one would
    // store a live secret where nothing can use it.
    expect(
      formFor({ row: "managed", action: "add" }, { mode: "managed", credentialSource: "attested" }),
    ).toMatchObject({ credential: "composio-token", keyNoun: "token" });
    expect(
      formFor({ row: "byok", action: "add" }, { mode: "managed", credentialSource: "attested" }),
    ).toMatchObject({ credential: "composio-api-key", keyNoun: "key" });
  });

  it("never lets one row offer two forms, for any status", () => {
    // The property the six-input predicate was approximating. A single nullable
    // return makes "two surfaces at once" unrepresentable, and the console holds
    // one `pending` at a time; what this walks is the other half — that add and
    // replace never both become permitted on the same row, which is what would
    // put two fields for one credential a click apart.
    const requests: ComposioPending[] = [
      { row: "managed", action: "add" },
      { row: "managed", action: "replace" },
      { row: "byok", action: "add" },
      { row: "byok", action: "replace" },
    ];

    for (const mode of ["managed", "byok", undefined] as const) {
      for (const credentialSource of ["attested", "company", "static", "none"] as const) {
        const perRow = new Map<string, number>();
        for (const pending of requests) {
          const form = formFor(pending, { mode, credentialSource });
          if (form) perRow.set(form.row, (perRow.get(form.row) ?? 0) + 1);
        }
        for (const [row, count] of perRow) {
          expect(count, `${row} under ${mode}/${credentialSource}`).toBe(1);
        }
      }
    }
  });
});

describe("a viewer who cannot manage", () => {
  it("is not a question the rows answer", () => {
    // `canManage` used to be one of the predicate's six inputs. It is a property
    // of the VIEWER, not of the row, and folding it in there is what made the
    // old function hard to reason about: it answered "may this person act" and
    // "would this control do anything" with one boolean. The component applies
    // it as a `disabled`; the rows say only what is possible.
    const rows = composioRows(status({ mode: "managed", credentialSource: "attested" }));
    expect(rows.every((row) => !("canManage" in row))).toBe(true);
    expect(formFor({ row: "managed", action: "add" })).not.toBeNull();
  });
});
