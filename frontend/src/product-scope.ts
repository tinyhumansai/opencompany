// Surfaces hidden while the product is scoped to one company per install.
//
// Every flag here hides a control; none of them changes a stored value or a
// server path. `ComposioMode::Managed`, the inference `managed` legacy alias
// and the hosts registry all still exist and still resolve exactly as before,
// so a company already on a hidden setting keeps working and re-enabling a
// surface is a single edit in this file.

/**
 * Hides the host roster, "Add a host" and "Manage hosts" in the switcher.
 *
 * Off. One console holding several hosts is the arrangement the connections
 * layer was built for, and it is not the same claim as "one company per
 * install": a host is *where* a company runs, so being able to point at
 * another one is how somebody moves off a laptop and onto a gateway at all.
 * Hiding it left the title row's company name as a plain `<div>` — the
 * nameplate branch — which is a control that looks like a control and opens
 * nothing.
 *
 * The browser keeps the half it can honour and loses the half it cannot: a
 * page can hold connections to any number of hosts, and cannot *start* one,
 * so `availableConnectors` still offers `local` and `ssh` only on the desktop
 * (`connections/types.ts`). Nothing here changes that split.
 */
export const HOSTS_HIDDEN = false;

/**
 * Hides the composer's intent group — "Just chatting" / "Do it once" /
 * "Build me the workflow".
 *
 * Hidden for now, and only the control is: `intent` starts `undefined` because
 * none of the three was ever pre-pressed (issue #1152), so a composer with the
 * group hidden sends exactly what a composer whose operator never pressed one
 * sends. Nothing downstream needs a branch, `deliverableChoice` still decides
 * which targets *could* offer it, and turning the row back on is this one
 * edit.
 */
export const COMPOSER_INTENT_HIDDEN = true;

/** Hides company switching, "All companies…" and "New company". */
export const COMPANY_SWITCHING_HIDDEN = true;

/** Hides the wizard's Advanced → Host group (bind address, workspace quotas). */
export const HOST_SETTINGS_HIDDEN = true;

/**
 * Hides the OpenHuman-managed Composio route, leaving BYOK the only choice.
 *
 * **Off**, for the same reason {@link INFERENCE_MANAGED_HIDDEN} is. The managed
 * route was hidden while choosing it meant going and minting a credential by
 * hand, which made a Composio API key of your own the honestly easier option.
 * The company-key grant removes that errand: the managed route is one click,
 * and it is the option that also arms the company's connections in the same
 * step.
 *
 * It used to gate the company-credential card as well, which made it one flag
 * doing two jobs — hiding a *Composio route* and hiding the *TinyHumans key*
 * surface. Those came apart when the grant landed and the card started deciding
 * its own visibility from the host's answer; see `CompanyCredentialCard`. What
 * is left here is the route, and the route is now offered.
 *
 * What this turns back on: `composioRows` returns a managed row that can be
 * selected rather than only reported, and `IntegrationStep` names the
 * TinyHumans account key alongside a Composio token as ways to finish
 * onboarding.
 */
export const COMPOSIO_MANAGED_HIDDEN = false;

/**
 * Hides the managed inference provider, leaving the operator to name one.
 *
 * Off for the same reason as {@link COMPOSIO_MANAGED_HIDDEN}: "Managed
 * (TinyHumans)" was hidden while choosing it meant going and minting a key by
 * hand, which made OpenRouter the honestly easier option. With the grant it is
 * one click, and it is the only option that also arms the company's connections
 * in the same step.
 */
export const INFERENCE_MANAGED_HIDDEN = false;
