// The exact, actionable reason a fail-closed turn could not run (keys
// rework, issue #2306, round-2 review KR-L2-03).
//
// Before this, a pinned provider switched off, a broken company default, or
// no model chosen at all all rendered the same generic "This turn couldn't
// be finished — something went wrong or a step took too long" line — even
// though the host had already computed the exact X9 sentence
// (`src/company/inference/copy.rs`) and logged it at the point of failure.
// The operator could read server logs to find the real answer; nothing on
// the surface they were actually looking at said it.
//
// Field names match `docs/key-reworks/in-use-guards.md` §5, the host's own
// contract: `userFacing`, `code`, `message`, `pairAgentId` and
// `providerSlug`. `pairAgentId` (round-3, handover doc §2.1/§4.2's field
// rename) rather than `agentId` — a reply already has an author, and a name
// this close to it invited reading "the agent who replied" instead of "the
// agent whose pair this failure is about" (the two differ for a title/triage
// pass running under a different agent's identity than the one it serves).
// Align further if `in-use-guards.md` §5 changes again.

import { connectionsHref } from "@/views/connection-pages";
import { consoleHref } from "./console-paths";

/**
 * The cases the contract names. Kept as a list rather than a type union on
 * {@link TurnFailure.code}: an unrecognised code must still render `message`
 * and fall back to the safe default action, never crash or hide the sentence
 * a person is relying on.
 */
export const TURN_FAILURE_CODES = [
  "no_model_chosen",
  "pair_provider_removed",
  "pair_provider_off",
  "default_provider_removed",
  "default_provider_off",
  "provider_no_key",
  "model_not_listed",
  "harness_unavailable",
] as const;

/**
 * The turn never reached a model at all: the teammate is bound to a harness
 * this host has no engine for, or to one whose warm-up failed. Its fix is on
 * the same Model tab a `pair_*` code points at — the tab that owns both the
 * harness and the model — so it shares that bucket in
 * {@link turnFailureAction} despite not being a `pair_` code.
 */
export const HARNESS_UNAVAILABLE_CODE = "harness_unavailable";

/**
 * A fail-closed turn's structured reason, once the host computed one of its
 * own X9 sentences for it — present on a company reply only for exactly
 * these cases, never for a generic provider or network failure (those keep
 * whatever prose the reply's own `text` already carries).
 */
export interface TurnFailure {
  /** Whether `message` is the exact sentence to show a person, rather than a diagnostic meant for logs. */
  userFacing: boolean;
  /** One of {@link TURN_FAILURE_CODES}, or an unrecognised string from a newer host. */
  code: string;
  /** The X9 sentence itself, with display names (X7) — render verbatim, never re-derived or paraphrased. */
  message: string;
  /** The agent this failure's PAIR names, when the code names one — never the reply's own author/channel, which is a different question. */
  pairAgentId?: string;
  /** The provider slug the failure names, when there is one. */
  providerSlug?: string;
}

/** The flat wire shape these fields arrive in, on whichever reply DTO carries them. */
export interface TurnFailureWire {
  userFacing?: boolean;
  code?: string;
  message?: string;
  pairAgentId?: string;
  providerSlug?: string;
}

/**
 * Builds a {@link TurnFailure} from the wire fields, or `undefined` when they
 * do not actually describe one.
 *
 * Requires `userFacing === true` and a non-blank `code` and `message` —
 * partial data (a `code` with no `message`, say, from a future host revision
 * still landing) is not enough to render a confident sentence, and falls
 * back to the generic text exactly as an older host's silence does today.
 */
export function toTurnFailure(wire: TurnFailureWire | null | undefined): TurnFailure | undefined {
  if (!wire || wire.userFacing !== true) return undefined;
  const code = wire.code?.trim();
  const message = wire.message?.trim();
  if (!code || !message) return undefined;
  return {
    userFacing: true,
    code,
    message,
    pairAgentId: wire.pairAgentId,
    providerSlug: wire.providerSlug,
  };
}

/**
 * Where the failed-turn bubble's action button should go, or `null` when
 * there is nothing useful to link — a pair code naming no agent id, which
 * only an unrecognised or malformed payload should ever produce.
 *
 * Two buckets, matching the contract's own two-way split and the codes'
 * own naming convention: a `pair_*` code — and `harness_unavailable`, whose
 * fix lives on the same tab — is a fix on that agent's own settings (Team →
 * the agent → Model); every other code — the company default, "no model
 * chosen" at all, a keyless provider, or a model the catalogue no longer
 * lists — is a fix on the company-wide LLM page.
 */
export function turnFailureAction(
  failure: Pick<TurnFailure, "code" | "pairAgentId">,
): { label: string; href: string } | null {
  if (failure.code.startsWith("pair_") || failure.code === HARNESS_UNAVAILABLE_CODE) {
    if (!failure.pairAgentId) return null;
    return { label: "Open Model settings", href: `${consoleHref("team", failure.pairAgentId)}?tab=model` };
  }
  return { label: "Open LLM settings", href: connectionsHref("inference") };
}
