#!/bin/sh
#
# Assert that every integration target under `tests/` actually executes tests
# in the CI lane that builds it. Issue #475.
#
# The failure this exists to catch is not a red test. It is a target that
# compiles, links, runs and reports zero. A file whose crate-level attribute is
# `#![cfg(feature = "openhuman")]` produces a perfectly healthy test binary in a
# lane without that feature — it just contains nothing. `cargo test` prints
# `0 passed; 0 failed` and exits 0, and a suite everybody believes is guarding
# an invariant guards nothing. That is how `tests/one_card_per_message.rs`, the
# 11-case regression suite for #463, sat in the tree with no runner: the default
# lane had the target but not the feature, the gated lane had the feature but
# passed `--lib`, which selects no target under `tests/` at all.
#
# So this checks a NUMBER, not a configuration. Configuration can look right and
# be vacuous; a count cannot.
#
# Usage:
#   scripts/ci/assert-integration-targets-run.sh [features]
#
# `features` is the comma-separated feature list of the lane being asserted,
# defaulting to the set the gated CI job builds. Cargo features are additive and
# that set is a superset of the default one, so a target that runs in the `Rust`
# job necessarily runs here too — asserting on one lane covers them all.
#
# A target that legitimately needs a feature this lane does not have will fail
# here. That is the intended outcome, and the message at the bottom says what to
# do about it: give this lane the feature, or add a lane that has it. Do NOT
# relax the target's `cfg` to make this pass — a test that runs only because it
# was stripped of the code it needs is the same vacuum in a different disguise.
#
# Requires `jq` (present on `ubuntu-latest`; `brew install jq` locally).

set -eu

FEATURES="${1:-openhuman,tinycortex}"

SCRIPT_DIR=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
REPO_ROOT=$(CDPATH='' cd -- "${SCRIPT_DIR}/../.." && pwd)
cd "${REPO_ROOT}"

if ! command -v jq > /dev/null 2>&1; then
  echo "assert-integration-targets-run: jq is required but not installed" >&2
  exit 1
fi

WORK=$(mktemp -d)
trap 'rm -rf "${WORK}"' EXIT

# `cargo metadata` rather than a glob over `tests/*.rs`: the manifest decides
# what a target is, and an explicit `[[test]]` entry may name a path this
# directory does not contain.
cargo metadata --locked --no-deps --format-version 1 \
  | jq -r '.packages[] | select(.name == "opencompany") | .targets[]
           | select(.kind | index("test")) | .name' \
  | sort > "${WORK}/targets"

if [ ! -s "${WORK}/targets" ]; then
  echo "No integration targets under tests/. Nothing to assert."
  exit 0
fi

echo "Integration targets under --features ${FEATURES}:"
echo

failed=0

# Redirected from a file, not piped: a `while` on the right of a pipe runs in a
# subshell and `failed` would not survive the loop.
while IFS= read -r target; do
  [ -n "${target}" ] || continue

  if ! cargo test --locked --features "${FEATURES}" --test "${target}" -- --list \
    > "${WORK}/listing" 2>&1; then
    cat "${WORK}/listing" >&2
    echo >&2
    echo "  ${target}: FAILED TO BUILD under --features ${FEATURES}" >&2
    failed=1
    continue
  fi

  # `--list` prints one `some::test_name: test` line per test case.
  count=$(grep -c ': test$' "${WORK}/listing" || true)

  if [ "${count}" -eq 0 ]; then
    echo "  ${target}: 0 tests  <-- RUNS NOWHERE"
    failed=1
  else
    echo "  ${target}: ${count} tests"
  fi
done < "${WORK}/targets"

echo

if [ "${failed}" -ne 0 ]; then
  cat >&2 << EOF
One or more integration targets executed nothing in this lane.

A target that builds but reports zero tests is not covered by CI, however green
the run looks. Its crate-level cfg is unsatisfied by --features ${FEATURES}, so
CI compiles an empty binary and reports success.

Fix the WIRING, not the test:

  * add the feature the target needs to the gated job in
    .github/workflows/ci.yml, if it belongs on that lane; or
  * add a job that builds the feature set the target needs, and run this script
    there too with that job's feature list.

Do not delete or weaken the target's #![cfg(...)] to make this pass. That
removes the code under test rather than running it, which is the failure this
check exists to name.
EOF
  exit 1
fi

echo "Every integration target executed at least one test."
