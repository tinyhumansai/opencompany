# Hive-mind integration: handoff (2026-09-06)

State of the `hivemind` branch for whoever picks this up on another machine.
Draft PR: https://github.com/tinyhumansai/opencompany/pull/2097 (fork branch
`senamakel:hivemind` → `tinyhumansai/opencompany` `main`). The PR body is the
long-form summary of what was built and why; this file is the operational
state.

## Where things stand

- **The merge of `upstream/main` is committed and pushed** (`983d67add`), and
  every gate below is green on it. `vendor/tinyhivemind` stays at `e272c84`
  (tinyhivemind `main`).
- **Cross-desk referral is wired** (`src/hivemind/referral.rs` and
  `docs/spec/runtime/hivemind-referral.md`). A desk that writes
  `[group_chat.hive.referral] enabled = true` may put one question to another
  desk; the far desk answers with a real turn on its own channel and the answer
  comes home under `hive-referral`, which folds as a system row so it can never
  be counted as a supporter. Off by default. `MentionTurnQueue` is reached
  through `reach = "local"` rather than wired separately — the library asserts
  that decision is identical, and two queues that must agree is worse than one.
- Desktop CI needed `crates/opencompany-app/Cargo.lock` refreshed for the new
  `tinyhivemind-hive` dependency, and `scripts/__pycache__` was committed by
  accident; both are fixed, with `__pycache__/` now ignored.
- An OpenHuman bump is a migration (harness APIs move); fix our side, never
  `vendor/`.

## Gates (all green on the merged tree, with referral)

```
cargo fmt --all -- --check
cargo clippy --all-targets --features openhuman -- -D warnings
cargo clippy --all-targets -- -D warnings
cargo test --features openhuman            # 6394 lib + every tests/ target
scripts/ci/assert-feature-lanes.sh
scripts/ci/assert-integration-targets-run.sh openhuman
```

Focused targets: `cargo test --features openhuman hivemind` (57),
`--test hivemind_e2e` (9), `brain::` (208), `store::memory` (cortexdb 10),
`memory_engine`, `manifest`, `--lib content_test`.

Long foreground commands get reaped by the harness on that box; detach with
`setsid nohup … > file &` and poll the file. `pkill -f <pattern>` matches the
shell's own command line and kills it (exit 144); kill by pid instead. The
shell cwd resets to the primary checkout between commands, so `cd` into the
worktree first.

## Live run recipe

```
scripts/cortexdb-up.sh                                  # container opencompany-cortexdb on 127.0.0.1:3141
set -a; . ~/.config/opencompany/cortexdb.env; set +a    # CORTEX_API_KEY
OPENCOMPANY_MEMORY=remote OPENCOMPANY_MEMORY_DRIVER=cortexdb \
OPENCOMPANY_MEMORY_URL=http://127.0.0.1:3141 OPENCOMPANY_MEMORY_API_KEY=$CORTEX_API_KEY \
OPENCOMPANY_INFERENCE_URL=http://127.0.0.1:6970/v1 OPENCOMPANY_INFERENCE_KEY=$LADDER_API_KEY \
OPENCOMPANY_INFERENCE_MAX_TOKENS=65536 OPENHUMAN_AGENT_TURN_TIMEOUT_SECS=1500 \
OPENCOMPANY_AUTH_MODE=none OPENCOMPANY_DATA_DIR=$HOME/.opencompany/hive-live-2 \
OPENCOMPANY_BIND=127.0.0.1:8080 \
  ./target/debug/opencompany serve --company companies/hive_math_lab
python3 scripts/hive-euler.py --problems 233,249,301,345 --timeout 5400 --out euler.json
```

The inference endpoint is a local llm-ladder-router. The shared `ladder`
container on `:6969` serves `flash` / `reasoning` / `max-reasoning` / `scribe`
and **no** `-v1` aliases, so a hive run against it fails every turn with
`unknown ladder chat-v1`: `build.rs::model_for_tier` emits the tier names, not
the ladder names. The router has no `aliases` key — it rejects one at parse
time — so the alias has to be a duplicated `[[ladders]]` block under the tier
name. Run a *second* container rather than editing the shared one:

```
sed 's/0.0.0.0:6969/0.0.0.0:6970/' ~/.config/ladder/config.toml > /tmp/hive-ladder.toml
# then append copies of the flash / reasoning / max-reasoning blocks renamed
# chat-v1 / reasoning-v1 / agentic-v1 (and vision-v1), and:
docker run -d --name ladder-hive -p 127.0.0.1:6970:6970 \
  -v /tmp/hive-ladder.toml:/etc/ladder/config.toml:ro \
  -e LADDER_API_KEY -e SURPLUS_API_KEY -e OPENROUTER_API_KEY \
  ghcr.io/senamakel/llm-ladder-router:latest --config /etc/ladder/config.toml
```

Point `OPENCOMPANY_INFERENCE_URL` at `:6970`. Grading also needs
`lucky-bai/projecteuler-solutions` cloned to `~/work/projecteuler-solutions`;
without it the driver runs the ladder and reports every verdict as ungradeable.
A BYOK `[inference] provider` in the manifest would need its key in the secret
store (`inference/key`); the env path is simpler for a lab.

Approvals: `shell` never parked on this policy (`mode = "full"`) during any
run; the driver still pumps `GET/POST /api/v1/company/approvals` concurrently
with the blocking chat POST.

## Live results so far

| run | seats | problems | result |
|---|---|---|---|
| 1 | 3, one model, anyone may propose | 1,5,12,14,31,60,92,100,145 | 8 correct in 4 turns each; 145 lost to the 10-min turn ceiling. Three independent proposals + vote every time, no cross-agent moves. |
| 2 | 6, `moves`, `require_evidential`, quorum 3, per-tier models | 12 | Exhausted: quorum reached at the 3rd evidential support, then the fold gave the Commit floor to seats barred from `!commit` for 8 turns. Fixed: commit is never gated. |
| 3 | same + commit fix + topic-id/citation discipline | 12,145,206,214 | all correct, 9 turns each (4 evidence, 2 support, 1 defer, 1 commit; supports cite evidence; verifier corrected a wrong figure; archivist defers with memory). |
| 3 | | 233 | strong seats lost turns: theorist and verifier to the 16k output cap (`finish_reason: length`), programmer to the 25-min ceiling; room continued; run was cut by session restarts before a close. |
| 4 | + `OPENCOMPANY_INFERENCE_MAX_TOKENS=65536` | 233,249,301,345 | started 2026-09-06 ~12:10 local on the original box; check `~/.opencompany/hive-live-2` journal / the PR for outcome. |

Transcripts are readable with
`GET /api/v1/company/chat/history?desk=solvers&limit=200` on a running serve
over the same data dir.

## What the hive does and does not do yet

Does: one shared transcript per desk; salience-bid floor; blind opening round;
evidence/support/object/defer/pin grammar with per-member gating; evidential
quorum; commit phase open to all; failed turns tolerated; desk memory recalled
before and written after each episode via CortexDB; per-seat model tiers.

Does, off by default: cross-desk referral — one question per line to another
desk, answered by one real turn there, with the answer carried home as a system
row that cannot be counted as support (`hivemind-referral.md`). Mention dispatch
is the same path at `reach = "local"`.

Does not: chain a referral deeper than one question and one answer — a referred
turn cannot itself refer. `!object` traffic has not appeared live because every
seat solved the rungs alone; it needs problems where seats disagree. Referral
has no live run behind it at all, only tests.

## Suggested next steps

1. Add a "trap" problem set where a literal misreading gives a different
   integer, to exercise the skeptic's `!object` path. `!object` has still never
   appeared live, because on every rung so far each seat reached the right
   number alone.
2. Consider `reasoning_effort` on the ladder's `agentic-v1` alias if
   `finish_reason: length` recurs even at 65536.
3. Give `hive_math_lab` a second desk and turn referral on, so the mechanism
   has a live run behind it as well as a scripted one. It has none yet: every
   claim in `hivemind-referral.md` is asserted by tests, not measured.
