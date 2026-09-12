// The shapes the Composio credential surface passes around.
//
// Types only — no values, no behaviour. Kept apart from `rows.ts` and
// `classify.ts` so those stay readable as *decisions*, which is the whole
// reason they are separate from the component that renders them.
//
// The credential rule, restated here because this is where somebody would add a
// field for convenience: **no shape in this file carries a credential.** The
// wire reports a *tier name* (`ComposioCredentialSource`) and nothing else, and
// the tier is what the rows are driven from. A boolean about whether a secret
// slot is filled is the shape of issue #886, which painted a working hosted
// tenant red because it answered about the first of three tiers.

import type {
  ComposioCredentialSource,
  ComposioMode,
  ComposioProbeClass,
} from "@/api/composio";

// Re-exported rather than redefined. These three are wire shapes, they already
// have one definition apiece in `api/composio.ts`, and a second copy here would
// be two things to keep in step for the sake of a shorter import. What this
// gives the console modules is one import surface for the whole subsystem.
export type { ComposioCredentialSource, ComposioMode, ComposioProbeClass };

/**
 * Which of the two routes a row stands for.
 *
 * The same values as {@link ComposioMode}, and that is not a coincidence:
 * `composio/mode` is a single stored scalar and `resolve_access` reads exactly
 * one branch, so the rows and the routes are the same set by construction.
 */
export type ComposioRowId = ComposioMode;

/** How a row's sub-line reads: an ordinary fact, or a fail-closed warning. */
export type ComposioRowTone = "muted" | "warning";

/**
 * Which word a row uses for the credential it holds.
 *
 * The two rows hold two different credentials and the console must never let
 * them read as one: the managed row holds a **token** the *TinyHumans backend*
 * recognises, and the own-account row holds an API **key** *Composio* itself
 * recognises. They authenticate different hosts and they are stored through
 * different routes — see `setComposioToken` vs `setComposioApiKey`, which say
 * so at length.
 */
export type ComposioKeyNoun = "token" | "key";

/**
 * Which controls a row is permitted to offer.
 *
 * **Permitted, not enabled.** Whether this viewer may act is `canManage`, which
 * is a property of the viewer rather than of the row, so it stays out of here
 * and is applied as a `disabled` by the component. What this object answers is
 * the harder question: would the control *do* anything. The rule the whole
 * design turns on is "do not render a control that cannot act", and every
 * `false` below is one such control not being rendered.
 */
export interface ComposioRowControls {
  /**
   * Offer "Use this" — move this company onto this row's route.
   *
   * Single-select, not a per-row toggle. Inference rows carry an independent
   * toggle because providers **coexist**; Composio has no coexistence, so a
   * toggle on each of two mutually exclusive rows creates reachable impossible
   * states (both on, both off). See `composioRows` for the full argument.
   */
  select: boolean;
  /** Open this row's credential form with nothing stored. */
  addKey: boolean;
  /** Open this row's credential form to rotate what is stored. */
  replaceKey: boolean;
  /** Clear the credential stored **for this row**, falling back to whatever remains. */
  removeKey: boolean;
  /** Probe the stored credential in place. */
  test: boolean;
}

/**
 * One row of the Connected card: a mark, a name, one sub-line and its controls.
 *
 * One fact on the sub-line, chosen by what the row is — three facts stacked
 * would be a table, and an operator is scanning for the row rather than reading
 * it.
 */
export interface ComposioRow {
  id: ComposioRowId;
  /** The route's name, as the operator reads it. */
  label: string;
  /** Whether this is the route the company is on right now. */
  active: boolean;
  /** The badge on the active row, or `null`. */
  badge: string | null;
  /** The one fact this row reports about itself. */
  subline: string;
  tone: ComposioRowTone;
  /** Which word this row's credential controls use. */
  keyNoun: ComposioKeyNoun;
  controls: ComposioRowControls;
}

/** A credential form the operator has asked for, before it is checked against the rows. */
export interface ComposioPending {
  row: ComposioRowId;
  /** `add` opens an empty field; `replace` rotates what is stored. */
  action: "add" | "replace";
}

/**
 * The credential form to render, once a {@link ComposioPending} has been
 * checked against the rows.
 *
 * **At most one, by type.** The predicate this replaces (`showManagedTokenCard`)
 * existed to stop two credential surfaces being on screen at the same moment an
 * operator was switching between routes — each direction broke differently, and
 * one of them silently destroyed a preserved token. A single nullable value
 * makes that unrepresentable rather than merely tested.
 */
export interface ComposioForm {
  row: ComposioRowId;
  /** Which write this form's Save performs. */
  credential: "composio-api-key" | "composio-token";
  keyNoun: ComposioKeyNoun;
  /** Whether an existing credential is being rotated rather than set for the first time. */
  rotating: boolean;
}
