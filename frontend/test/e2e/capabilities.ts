/**
 * What the host under test can actually do, so a spec that needs more than the
 * default build can say so instead of failing (issue #428).
 *
 * # Why these are opt-in variables and not a probe of the host
 *
 * A probe would be better, and there is very nearly one: `GET /tiny` reports
 * the vendored runtime modules. It cannot be used for this. `openhuman` is
 * reported as `enabled: true` unconditionally — the field describes the
 * vendored *checkout*, not the `openhuman` cargo feature — so a default build
 * answers exactly like a feature-gated one. A probe that cannot distinguish the
 * two would skip nothing, or skip everything, and would look authoritative
 * while doing it.
 *
 * So the caller declares what it brought. That is honest about where the
 * knowledge lives: whoever built the binary and started the inference backend
 * is the only party that knows.
 *
 * # These skips are not permission to leave the specs unrun
 *
 * Four of the suite's best specs sit behind `LIVE_BRAIN`, and they are the ones
 * that exercise the product rather than the console's own rendering. Skipping
 * them buys a default-feature lane that is meaningfully green — a real gate,
 * today — and nothing more. Issue #467 tracks standing the feature-gated lane
 * up so they run for real; every skip below names it.
 */

/**
 * A host built with `--features openhuman,tinycortex` **and** an inference
 * backend behind it — either the mock that echoes `__MOCK_LLM__` or one whose
 * tool choices are scripted (`SPAWNONE`).
 *
 * Set `PW_LIVE_BRAIN=1` when both are true. Without them the agent harness is
 * not compiled in at all, so a sent message is answered by nothing, a workflow
 * node with no inference source never runs, and no orchestrator exists to open
 * a card.
 */
export const LIVE_BRAIN = process.env.PW_LIVE_BRAIN === "1";

// `PW_MCP_SERVER` used to live here: a path to a local stdio MCP server the MCP
// spec installed and called. Both are gone. The console's MCP page installed it
// through routes no host serves (issue #414), and the host rejects stdio servers
// outright in hosted v1 — so the capability gated a spec that could not have
// passed had the fixture been supplied. The MCP spec now drives the real
// surface against the default-feature host and needs nothing extra.

/** The reason string a `LIVE_BRAIN` skip carries, so no skip is ever bare. */
export const LIVE_BRAIN_REASON =
  "needs a --features openhuman,tinycortex host plus an inference backend; " +
  "set PW_LIVE_BRAIN=1 to run. Tracked by issue #467.";
