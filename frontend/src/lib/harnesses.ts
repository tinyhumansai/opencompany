// Joining what a company can bind to against what this machine can actually
// run (issue #1245's detected-harness follow-up).
//
// Two half-answers, from two places that cannot see each other:
//
//   - the host says *which ids are bindable* — declared `[[harness]]` entries
//     plus every coding CLI this build drives (`HarnessDto`). It cannot say
//     whether any of them is installed, because that is a fact about whichever
//     machine is asking.
//   - the desktop says *what is installed and signed in here*
//     (`AcpHarnessStatus`, from `acp::discovery`). It knows nothing about the
//     company.
//
// The id is the join key, and that it works at all is not an accident: the
// manifest's `ACP_AGENTS` vocabulary and the discovery catalogue are the same
// two ids (`claude`, `codex`), deliberately.

import type { AcpHarnessStatus, AcpReadiness } from "@/api/transport/desktop";
import type { HarnessDto } from "@/api/types";

/** One harness as the External harnesses page renders it. */
export interface HarnessRow {
  id: string;
  /** The friendly name when this machine knows it, else the raw id. */
  label: string;
  kind: HarnessDto["kind"];
  /** Whether an agent naming no harness lands here. */
  isDefault: boolean;
  /** Declared in `company.toml`, versus detected as a drivable CLI. */
  declared: boolean;
  /** `local` / `runner`, for an `acp` harness. */
  transport?: string;
  /**
   * The ACP agent this harness drives — `claude`, `codex` — as distinct from
   * `id`, which is whatever the manifest called the binding.
   *
   * Kept on the row because **every** desktop call is keyed on this, not on
   * `id`: the shell's catalogue only knows the agent names. A declared
   * `id = "laptop", agent = "claude"` addressed by id gets "not a harness this
   * build knows" back from `confirmAcpHarness`, and an empty model list from
   * `ensureAcpModels`. `id` remains the company-side binding key.
   */
  agent?: string;
  /**
   * What this machine says about it, or `undefined` when nothing can say —
   * a browser (no local probe), or a harness that is not a local CLI at all.
   *
   * `undefined` is **not** "not installed": those are different facts, and
   * rendering the first as the second would tell someone to install a CLI
   * they already have, on a machine that simply was not asked.
   */
  readiness?: AcpReadiness;
  /**
   * Where the adapter turned out to be.
   *
   * Absent until a confirmation lands, and absent afterwards on every outcome
   * that has no use for it — the survey resolves no paths, because it looks
   * nothing up.
   */
  path?: string;
}

/**
 * The joined view, declared entries first and detected ones after — the order
 * the host already returns, preserved so the page does not reshuffle a list
 * the operator may be reading against the manifest.
 *
 * `local` is the desktop's survey, or `null` in a browser where nothing can be
 * probed. Passing `null` is what produces the `readiness: undefined` rows the
 * page renders as "can't say from here" rather than as "missing".
 *
 * The survey carries no paths and no verdicts — every local row it names
 * arrives `checking`. Both arrive later, together, via {@link withReadiness}.
 */
export function joinHarnesses(
  declared: HarnessDto[],
  local: AcpHarnessStatus[] | null,
): HarnessRow[] {
  const byId = new Map((local ?? []).map((h) => [h.id, h]));
  return declared.map((harness) => {
    // Probeable when the **host** says this machine runs it, not when the
    // shape looks local. A `runner` harness names a CLI on somebody else's
    // machine and a `built_in` one has no CLI at all — but the case that
    // needed the host's answer is subtler: a desktop connected to a remote
    // company sees a declared `transport = "local"` harness that is local to
    // *that host*, not to the laptop running this console. Probing it here
    // reported readiness — and offered an install — for a machine that will
    // never run those turns.
    //
    // A host that does not send `runsHere` is not probed at all. That renders
    // as "can't say from here", which is honest, where the opposite default
    // would resurrect exactly the bug this replaced.
    const probeable =
      harness.runsHere === true && harness.kind === "acp" && harness.transport === "local";
    // Keyed on the **ACP agent**, not the harness id. They coincide for a
    // detected row (the host synthesizes it named after the agent) but not for
    // a declared one: a manifest is free to write
    // `[[harness]] id = "laptop", agent = "claude"`, and this machine's probe
    // catalogue only ever knows `claude`. Keying on the id left that row
    // reading "Desktop only" on a machine where Claude Code was ready.
    const found = probeable ? byId.get(harness.agent ?? harness.id) : undefined;
    return {
      id: harness.id,
      label: found?.label ?? harness.id,
      kind: harness.kind,
      isDefault: harness.default,
      declared: !harness.detected,
      transport: harness.transport,
      agent: harness.agent,
      readiness: found?.readiness,
    };
  });
}

/**
 * Whether a row can actually take a turn on this machine right now.
 *
 * A `built_in` harness always can — it is the host's own engine and needs
 * nothing installed. A local ACP one needs its CLI ready. Anything whose
 * readiness is unknown answers `false`, because "cannot say" must not read as
 * a promise the turn will work.
 */
export function isUsableHere(row: HarnessRow): boolean {
  if (row.kind === "built_in") return true;
  return row.readiness?.state === "ready";
}

/**
 * Whether this row is still waiting on a handshake to settle.
 *
 * The page uses it to decide what to confirm, and the footer to avoid
 * reporting a count while answers are still arriving — "1 of 3 can run a turn"
 * is a claim, and making it mid-probe would be wrong for as long as the slowest
 * CLI takes to start.
 */
export function isChecking(row: HarnessRow): boolean {
  return row.readiness?.state === "checking";
}

/**
 * The same rows with `id`'s readiness replaced — the reducer the page applies
 * as each confirmation lands.
 *
 * A pure swap rather than a mutation so a stale probe (the operator hit
 * "Check again" while one was still in flight) cannot resurrect a row that a
 * newer survey has already replaced: the caller re-keys off its own current
 * state, and a row whose id is gone is simply not found.
 */
export function withReadiness(
  rows: HarnessRow[],
  id: string,
  readiness: AcpReadiness,
  path?: string,
): HarnessRow[] {
  // `path` arrives with the verdict rather than ahead of it: the survey looks
  // nothing up, so a row only learns where its adapter is once one has been
  // started, and only on the outcomes that quote it.
  return rows.map((row) => (row.id === id ? { ...row, readiness, path } : row));
}

/**
 * What this app can *do* about a row, as opposed to what it can say.
 *
 * Derived here rather than switched on again in the component: the note and
 * the button have to agree, and two independent switches over the same union
 * is how they drift — a row that reads "needs an add-on" beside no button, or
 * an Install button on a machine with no Node to run the result.
 */
export type HarnessAction = "install" | "update" | "none";

export function harnessAction(row: HarnessRow): HarnessAction {
  switch (row.readiness?.state) {
    case "adapterMissing":
      return "install";
    case "adapterOutdated":
      return "update";
    default:
      // Everything else is either fine, still resolving, or fixed by something
      // this app cannot do for them — signing in, installing a CLI, installing
      // Node. Offering a button there would be offering a no-op.
      return "none";
  }
}

/**
 * The id the **desktop** knows this harness by.
 *
 * Every `confirmAcpHarness` / `ensureAcpModels` call goes through this rather
 * than reaching for `.id` directly, because the two coincide for a detected
 * row and diverge for a declared one — which is exactly the case that is easy
 * to write and hard to notice.
 */
export function desktopHarnessId(row: Pick<HarnessRow, "id" | "agent">): string {
  return row.agent ?? row.id;
}

/**
 * The status dot and word for a row.
 *
 * The word carries the meaning and the dot is shorthand — the same rule
 * `host-switcher.tsx` follows, because hue alone tells someone that something
 * differs without saying what.
 *
 * Lives here rather than beside one of the two surfaces that render it, for
 * the same reason {@link harnessAction} does: the settings list and the agent
 * picker must not be able to call the same machine state two different names.
 */
export function statusOf(row: HarnessRow): { label: string; dot: string } {
  if (row.kind === "built_in") return { label: "Managed", dot: "bg-status-done" };
  if (row.transport === "runner") return { label: "Remote", dot: "bg-muted-foreground/50" };
  switch (row.readiness?.state) {
    case "checking":
      return { label: "Checking…", dot: "bg-status-running animate-pulse" };
    case "ready":
      return { label: "Ready", dot: "bg-status-done" };
    case "notSignedIn":
      return { label: "Not signed in", dot: "bg-status-blocked" };
    case "notInstalled":
      return { label: "Not installed", dot: "bg-muted-foreground/50" };
    case "adapterMissing":
      // Not "Not installed": the CLI *is* installed, and this app is one small
      // add-on away from being able to drive it. Saying otherwise is what sent
      // people to reinstall software they already had.
      return { label: "Add-on needed", dot: "bg-status-blocked" };
    case "adapterOutdated":
      return { label: "Update available", dot: "bg-status-blocked" };
    case "nodeMissing":
      return { label: "Needs Node.js", dot: "bg-muted-foreground/50" };
    case "spawnFailed":
      return { label: "Won't start", dot: "bg-destructive" };
    default:
      // Nothing probed this machine — a browser, or a desktop shell predating
      // `oc_acp_harnesses`. Deliberately not "Not installed": those are
      // different facts, and saying the second here would tell someone to
      // reinstall a CLI already sitting on their machine, unseen from a tab.
      return { label: "Desktop only", dot: "bg-muted-foreground/50" };
  }
}

/** What to tell the operator about a row, and what to do about it. */
export function readinessNote(row: HarnessRow): string {
  if (row.kind === "built_in") {
    return "Runs on this host's own credential — nothing to install.";
  }
  if (row.transport === "runner") {
    return "Runs on a registered remote machine, not this one.";
  }
  // The resolved binary, shown **only when something is wrong**.
  //
  // On a working harness it is noise: nobody needs a filesystem path to act
  // on "it works", and the path is the ACP *adapter* (`claude-agent-acp`),
  // not the `claude` anyone would recognise — so printing it next to a green
  // row invites "that isn't where my Claude Code lives" for no benefit.
  //
  // When it fails, the same string is the most useful fact on the row: it
  // says *which* install refused, and a machine with npm-global, Homebrew and
  // nvm copies of the same tool is ordinary.
  const at = row.path ? ` (${row.path})` : "";
  switch (row.readiness?.state) {
    case "checking":
      // Claims nothing, because nothing has been established. Two earlier
      // versions of this string each asserted a fact the code had not checked
      // — first "installed and signed in", then "found on PATH" — and both
      // read as reassurance right up until the handshake disagreed.
      return "Starting it to see if it answers…";
    case "ready":
      return "Installed, signed in, and answering.";
    case "notInstalled":
      return "Not found on PATH — install it to use this harness.";
    case "adapterMissing":
      // Names the CLI that *was* found, because the operator's first reaction
      // to any "missing" wording is that we failed to see an install they know
      // is there. Saying where it is settles that before asking for anything.
      //
      // No npm command to copy any more: the adapter is this app's dependency,
      // so the app installs it. The row carries a button.
      return `Found at ${row.readiness.cli} — it needs a small add-on to talk to this app.`;
    case "nodeMissing":
      // The one unready state with no button, because installing cannot fix
      // it. Naming Node explicitly is the whole value: "installation failed"
      // is what block/buzz showed here, and it sent people to reinstall an
      // adapter that was never the problem.
      return "Node.js 18 or newer is required to run a coding harness, and none was found.";
    case "adapterOutdated":
      return `Add-on ${row.readiness.found} is installed; this app expects ${row.readiness.want}.`;
    case "notSignedIn":
      return `Starts${at}, but refused the session — sign in to its CLI.`;
    case "spawnFailed":
      return `${row.readiness.reason}${at}`;
    default:
      // No probe ran, so nothing is known either way. Says which — "we did
      // not look" — rather than guessing in either direction, because the
      // wrong guess in each direction is its own bad advice: "not installed"
      // sends someone to reinstall a CLI they have, and "ready" promises a
      // turn that will fail.
      return "A coding CLI runs on your own machine, so only the desktop app can see it.";
  }
}

/** The roster fields {@link boundAgents} resolves a lane from. */
export interface HarnessBinding {
  id: string;
  /** The declared binding, absent when this teammate names none. */
  harness?: string;
}

/**
 * Which teammates run their turns on `harnessId`.
 *
 * `defaultHarnessId` is the id of the harness marked `default = true`, and it
 * is required rather than optional because the rule turns on it: a teammate
 * that declares no harness is served by the default one **and by no other**.
 * This is `agents_on` in `harness/lanes.rs`, resolved on the roster read rather
 * than duplicated as an endpoint — the count is a claim about which lane the
 * runtime will actually route to, so the two rules have to be the same rule.
 *
 * A company whose host names no default passes `undefined`, and an undeclared
 * teammate then matches nothing: where it lands is a fact this read does not
 * carry, and picking a row for it would be inventing one.
 */
export function boundAgents<T extends HarnessBinding>(
  members: readonly T[],
  harnessId: string,
  defaultHarnessId: string | undefined,
): T[] {
  return members.filter((member) => (member.harness ?? defaultHarnessId) === harnessId);
}

/**
 * How a harness reports its teammates.
 *
 * Deliberately not "connected", the word the Providers page uses for a
 * credential this company holds: a harness holds nothing company-wide, and
 * borrowing that word would promise a shared persisted state that does not
 * exist. Some number of teammates each picked it, which is what this says.
 */
export function boundLabel(count: number): string {
  if (count === 0) return "No agents bound";
  return `${count} agent${count === 1 ? "" : "s"} bound`;
}
