// What the Composio Connected card says, decided once.
//
// PURE. No React, no fetch, no `document`. Everything in here is a branch worth
// a test, which is exactly why it is not in the component: the page this
// replaces carried the same decisions inline, where the only way to reach one
// was to render six layers and read the result off a screen.

import type { ComposioCredentialSource, ComposioStatus } from "@/api/composio";
import { COMPOSIO_MANAGED_HIDDEN } from "@/product-scope";
import type {
  ComposioForm,
  ComposioMode,
  ComposioPending,
  ComposioRow,
} from "./types";

/**
 * The managed route's name, as the operator reads it.
 *
 * **TinyHumans, not OpenHuman.** It was the latter first, on the argument that
 * this row's chain resolves to three different payers — this company's
 * TinyHumans account, the instance identity, or a pasted token — so naming the
 * whole row after one of them would be false of the other two.
 *
 * That argument is about the *sub-line*, and the sub-line still makes exactly
 * that distinction: {@link managedSubline} names the payer per resolved tier
 * and is untouched. What the label answers is the coarser question the row is
 * there to ask — whose Composio account, ours or yours — and the answer to that
 * is the company an operator has an account and a balance with. "OpenHuman" is
 * the runtime; nobody is billed by it and it appears nowhere else an operator
 * pays.
 */
export const MANAGED_LABEL = "TinyHumans-managed";

/** The own-account route's name. */
export const BYOK_LABEL = "This company's own Composio account";

/** The badge the route a company is on carries **when it also resolves**. */
export const ACTIVE_BADGE = "Active";

/**
 * The badge for the route a company is on when **nothing resolves on it**.
 *
 * Selection and availability are two different truths and this row used to
 * render them as one: `active` answers "is this the route you picked", and it
 * handed out `Active` whenever it was true — beside a sub-line reading "No
 * credential resolves — agents cannot connect apps", in the amber the row
 * already chose because it knew. A green tick claiming a working route, three
 * inches from the sentence saying it does not work.
 *
 * Not solved by rendering the row as inactive, which is the opposite error:
 * turns really are routed here, and a row that looks unselected hides that.
 * "Selected" says the one thing that is true — you chose this — and leaves the
 * sub-line to say the rest.
 *
 * The inference surface settled the same question the same way: its Managed row
 * dropped a permanent "Always on" badge because that was a claim of
 * availability the row could not back.
 */
export const SELECTED_BADGE = "Selected";

/**
 * The route to render for whatever the host said.
 *
 * A host predating BYOK omits `mode`, and one from a future shape could name a
 * route this console has no copy for. Both read as `managed`: it is the only
 * route every host has, and the one whose controls are safe to offer when we do
 * not know.
 *
 * Moved here from the component it used to live in, unchanged — the component
 * kept it and its test reached it through an import of a `.tsx`, which is the
 * shape this whole module exists to stop.
 */
export function modeOf(
  status: ComposioStatus | null | undefined,
): ComposioMode {
  const mode = status?.mode;
  return mode === "managed" || mode === "byok" ? mode : "managed";
}

/**
 * What the **managed chain** resolves to, or `undefined` when nothing said.
 *
 * The fallback is narrow on purpose. Under `mode: "managed"` the host defines
 * `managedCredentialSource` to equal `credentialSource`, so an older host's
 * `credentialSource` is a *correct* stand-in there. Under `byok` the two are
 * about different chains and nothing on an older wire answers for the managed
 * one — so the answer is "not said", and the row says what managed *is* rather
 * than claiming a tier nobody established.
 */
export function managedSourceOf(
  status: ComposioStatus | null | undefined,
): ComposioCredentialSource | undefined {
  if (status?.managedCredentialSource) return status.managedCredentialSource;
  return modeOf(status) === "managed" ? status?.credentialSource : undefined;
}

/**
 * The managed row's sub-line.
 *
 * **Driven by the resolved tier, never by "did somebody paste a token".** That
 * boolean is issue #886: it answers only about the first of three tiers and is
 * routinely `false` on a working hosted tenant, so a row driven off it reports
 * a live connector as unconfigured.
 *
 * `company` and `attested` are deliberately **not collapsed**. One bills this
 * company's TinyHumans account and the other bills whoever runs the server, and
 * that is the decision an operator is on this page to make — a row that says
 * only "connected" hides that the move has not happened.
 */
export function managedSubline(
  source: ComposioCredentialSource | undefined,
): string {
  switch (source) {
    case "static":
      return "Using the Composio token saved for this company";
    case "company":
      return "Billed to this company's TinyHumans account";
    case "attested":
      return "Billed to whoever runs this server";
    case "none":
      return "No credential resolves — agents cannot connect apps";
    // An older host did not say, and "not said" is not "working". This says what
    // the route is rather than claiming a tier nobody established.
    default:
      return "Reached through the Composio account TinyHumans holds";
  }
}

/**
 * The host part of a URL, for a sub-line that has to be checkable rather than
 * merely asserted.
 *
 * Falls back to the raw string: a host that answered with something unparseable
 * is still telling the operator something, and swallowing it would leave the
 * row claiming a configured key with nowhere named.
 */
export function endpointHost(url: string | undefined): string {
  if (!url) return "";
  try {
    return new URL(url).host;
  } catch {
    return url;
  }
}

/**
 * Both rows of the Connected card, fully decided, in render order.
 *
 * # Why single-select and not a toggle per row
 *
 * The inference rows this page's language is borrowed from carry an independent
 * per-row toggle plus a "Default" marker, which models providers that
 * **coexist**: two of them can be enabled at once and one of them takes the
 * unrouted work. Composio has no coexistence — `composio/mode` is a single
 * stored scalar and `resolve_access` reads exactly one branch. A toggle on each
 * of two mutually exclusive rows therefore creates reachable impossible states
 * (both on, both off) with no stored value to put them in. That is the same
 * rule that removed the toggle from the Managed inference row, one level up.
 *
 * So: keep the row language, replace the toggle with single-select. The active
 * row carries an `Active` badge; the other offers `Use this`.
 *
 * # What is deliberately not offered
 *
 * - **`removeKey` on the own-account row** is permanently `false`, and it names
 *   a shape of the host rather than a decision anybody is free to reverse. Clearing a BYOK key is not "the key
 *   goes away": the host derives the route from whether a key exists, so
 *   `setComposioApiKey("")` writes the mode back to `managed` as a side effect.
 *   That is the same call the managed row's `Use this` makes, and rendering one
 *   action twice under two names is how an operator comes to believe they are
 *   two. The managed row *does* offer `removeKey`, because clearing the token
 *   stored for that route falls back to the company key or the instance
 *   identity and leaves the route where it was.
 */
export function composioRows(
  status: ComposioStatus | null | undefined,
): ComposioRow[] {
  const mode = modeOf(status);
  const managedSource = managedSourceOf(status);
  const onManaged = mode === "managed";

  // Under `byok` the pasted API key IS the credential, so `none` is the host
  // saying no key is stored — a real, reachable state that `resolve_access`
  // warns about and fails closed on rather than borrowing another identity.
  // Under `managed` the question does not arise: the host derives the route
  // from the key's existence, so a managed company has no BYOK key by
  // construction.
  const byokKeyStored = !onManaged && status?.credentialSource !== "none";

  // A token stored *for the managed route* — the `composio/token` override, a
  // different credential from the BYOK key and stored through a different
  // route. `static` is the only tier that means one exists.
  const managedTokenStored = managedSource === "static";

  // Whether anything actually answers on each route. One boolean per row,
  // because the badge and the tone are two renderings of this same fact and
  // they drifted apart when each derived it for itself: the tone knew the
  // managed chain resolved to nothing and went amber, while the badge next to
  // it still said "Active".
  //
  // `undefined` resolves. It is an older host that did not say, and "not said"
  // is not evidence of `none` — the same rule `managedSubline` follows.
  const managedResolves = managedSource !== "none";
  const byokResolves = byokKeyStored;

  const managed: ComposioRow = {
    id: "managed",
    label: MANAGED_LABEL,
    active: onManaged,
    // Selected is not working. See `SELECTED_BADGE`.
    badge: onManaged ? (managedResolves ? ACTIVE_BADGE : SELECTED_BADGE) : null,
    subline: managedSubline(managedSource),
    tone: managedResolves ? "muted" : "warning",
    keyNoun: "token",
    controls: {
      // Hidden when the managed chain resolves to nothing: switching to a route
      // that resolves to nothing is an outage, not a choice. Offered when
      // nothing was *said* (an older host), because hiding it there would take
      // away the only route back to managed on every host predating the field
      // — "not said" is not evidence of `none`.
      //
      // `COMPOSIO_MANAGED_HIDDEN` takes the way IN, and only that. The flag
      // promises "leaving BYOK the only choice", and until now the rows did not
      // read it at all: its one runtime consumer was onboarding copy, so
      // turning it back on as a rollback would have changed the instructions
      // and left every control that acts on the route exactly where it was.
      //
      // Taking the way in rather than the row is deliberate. A company already
      // ON the managed route still gets its row, because removing the checked
      // option from a radiogroup leaves every remaining radio reporting
      // `aria-checked="false"` — a control claiming the company has chosen
      // nothing, which is a different and wrong statement from "it is on a
      // route not offered here". That regression is pinned in
      // `product-scope-hidden-surfaces.test.ts`.
      select: !COMPOSIO_MANAGED_HIDDEN && !onManaged && managedSource !== "none",
      // Also offered from BYOK when the managed chain resolves to NOTHING, and
      // for a reason the `onManaged` half cannot cover: that is the one state
      // where `select` above is hidden, so without this the managed route is
      // unreachable in both directions at once — no "Use this" because it would
      // switch into an outage, and no way to store the token that would end the
      // outage. Writing `composio/token` does not move the company off BYOK, so
      // this provisions the prerequisite and leaves the active route alone;
      // `select` appears on the next status read.
      //
      // The flag gates only the BYOK-side half of that: provisioning a token
      // for a route that cannot then be chosen is an errand with no end. A
      // company already on managed keeps its token controls either way, for the
      // same reason it keeps its row — being on a route is not choosing it.
      addKey:
        !managedTokenStored &&
        (onManaged || (!COMPOSIO_MANAGED_HIDDEN && managedSource === "none")),
      replaceKey: onManaged && managedTokenStored,
      removeKey: onManaged && managedTokenStored,
      // Not offered on this row, and not for want of a button. The host's
      // check (`POST …/composio/api-key/test`) probes the **BYOK key** against
      // Composio; the managed route's credential is a bearer the TinyHumans
      // backend recognises, and there is no cheap call that tells a bad bearer
      // apart from a backend that is down. A Test here would report an outage
      // as a rejected credential, which is the exact misclassification the
      // whole probe design exists to avoid.
      test: false,
    },
  };

  const byok: ComposioRow = {
    id: "byok",
    label: BYOK_LABEL,
    active: !onManaged,
    // Same rule as the managed row above: a company can be on its own account
    // with no key stored, and that row must not wear a badge saying it works.
    badge: !onManaged ? (byokResolves ? ACTIVE_BADGE : SELECTED_BADGE) : null,
    subline: !onManaged
      ? byokKeyStored
        ? `•••• configured · ${endpointHost(status?.backendUrl)}`
        : "No API key stored — agents get no Composio tools"
      : "Not connected",
    tone: !onManaged && !byokResolves ? "warning" : "muted",
    keyNoun: "key",
    controls: {
      // Always reachable. Unlike managed, this route cannot resolve to nothing
      // by surprise — choosing it *is* pasting the credential that makes it
      // resolve, which is why `select` here hands off to the key form rather
      // than writing anything on its own.
      select: onManaged,
      addKey: !onManaged && !byokKeyStored,
      replaceKey: !onManaged && byokKeyStored,
      removeKey: false,
      // Offered exactly when there is a stored key to check. On the managed
      // route, or on this one with a blank slot, the host answers `409
      // not_configured` — a permanent state, so the honest rendering is no
      // control rather than one that can only fail.
      test: !onManaged && byokKeyStored,
    },
  };

  return [managed, byok];
}

/**
 * The credential dialog's heading.
 *
 * Four branches over five entry points — `managed`/`byok` × add/replace, plus
 * the own-account row's "Use this", which `composioForm` folds into that row's
 * add. A modal that says only "Add a token" leaves the operator to remember
 * which of two routes they clicked on, so this names the route rather than the
 * field; the field is labelled underneath it.
 *
 * Deliberately never the exact phrase "Composio token": the popup's accessible
 * name comes from this string, and a Playwright `getByLabel(/Composio token/)`
 * would then match both the dialog and the input it is looking for.
 */
export function credentialDialogTitle(form: ComposioForm): string {
  if (form.row === "byok") {
    return form.rotating
      ? "Replace this company's Composio API key"
      : "Connect this company's own Composio account";
  }
  return form.rotating
    ? "Replace the token for the managed route"
    : "Add a token for the managed route";
}

/**
 * The line under that heading: what storing this credential does.
 *
 * Says the consequence rather than repeating the field's own label, because
 * that is the half an operator cannot read off the form — one of these two
 * moves every agent in the company onto another Composio account.
 */
export function credentialDialogBlurb(form: ComposioForm): string {
  return form.row === "byok"
    ? "The key this company's agents will present to Composio, instead of the managed route's."
    : "Used for this company only, in place of whatever the managed route would otherwise resolve to.";
}

/**
 * The credential form to render, or `null`.
 *
 * Checked against the rows rather than trusted: a pending form is an intent
 * recorded at click time, and the status underneath it can move — a refresh, a
 * save, another admin. A "replace the key on the own-account row" form left
 * standing after the company moved to managed is a field whose Save writes a
 * credential for a route it is no longer on.
 *
 * `add` is permitted from either the row's own Add control **or** from a
 * `select` that is a credential hand-off. The own-account row has no way to be
 * chosen except by supplying the key that makes it resolve, so its "Use this"
 * opens this form rather than writing anything.
 */
export function composioForm(
  pending: ComposioPending | null,
  rows: readonly ComposioRow[],
): ComposioForm | null {
  if (!pending) return null;
  const row = rows.find((r) => r.id === pending.row);
  if (!row) return null;

  const permitted =
    pending.action === "replace"
      ? row.controls.replaceKey
      : row.controls.addKey || (row.id === "byok" && row.controls.select);
  if (!permitted) return null;

  return {
    row: row.id,
    credential: row.id === "byok" ? "composio-api-key" : "composio-token",
    keyNoun: row.keyNoun,
    rotating: pending.action === "replace",
  };
}
