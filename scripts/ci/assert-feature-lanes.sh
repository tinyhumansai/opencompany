#!/bin/sh
#
# Assert that every Cargo feature is either run by a CI lane or declared
# compile-only WITH A REASON — and that a compile-only declaration is true.
# Issue #770.
#
# The failure this exists to catch is not a red test. It is a feature-gated test
# that NOTHING SELECTS. Cargo features are additive and every test lane in
# ci.yml pins an explicit feature set, so the default fate of a gated test is
# "compiled by `Check (--all-features)`, executed by nothing" — `cargo test`
# prints no mention of it, no lane reports zero, and a suite everybody reads as
# coverage guards nothing. Six features had accumulated never-executed tests
# before this check existed, including the `sidecar` brain's entire offline
# end-to-end suite and two OAuth secret-lifecycle tests in `app::types`.
#
# It is the same shape as #475 (an integration target no lane selected), #477
# (a feature compiled and never run), #555 (the MongoDB conformance suite) and
# #592 (a submodule-init block copied into several lanes with one copy missed).
# Each was found by hand, after the fact. This is the check that finds the next
# one at the moment it is introduced.
#
# WHAT IT ASSERTS, per feature, against scripts/ci/feature-lanes.txt:
#
#   * every feature `cargo metadata` reports has a row (an unmapped feature is
#     RED — a new feature cannot merge until someone writes down which lane runs
#     its tests, or why none does);
#   * every row names a real feature (a stale row for a deleted feature is RED,
#     because a table that describes a tree that no longer exists is worse than
#     no table);
#   * `tested` / `partial` rows name a feature set that some `cargo test` line in
#     ci.yml actually enables, and `partial` rows additionally name filters that
#     appear there;
#   * `compile-only` rows carry a reason AND have no feature-gated test anywhere
#     under src/ or tests/. This is the load-bearing one: it is what makes
#     "deliberate" checkable rather than merely claimed.
#
# WHAT IT DOES NOT ASSERT, stated so nobody reads more into a green run than is
# there. For a `partial` row it cannot prove the filters SELECT every gated test
# the feature owns — a filter that misses one is invisible here. Two other
# things cover that from the runtime side: the per-step count assertions in
# ci.yml (a filter that selects nothing exits 0, which is why every lane's count
# is asserted non-zero) and scripts/ci/assert-integration-targets-run.sh. This
# script is the STATIC half — it proves a lane was declared, not that the lane
# is exhaustive. Keep the filters honest by hand.
#
# The three gated-test idioms in this tree, all three of which it detects:
#
#   (a) #[cfg(all(test, feature = "x"))]  on a test module or test-only helper
#   (b) #[cfg(feature = "x")] immediately above #[test] / #[tokio::test]
#   (c) #[cfg(feature = "x")] on a `mod y;` declaration, with the tests inside
#       that module's own file — this is how `sidecar` hid fourteen of them, so
#       it is detected rather than assumed away.
#
# Requires `jq` (present on ubuntu-latest; `brew install jq` locally).

set -eu

SCRIPT_DIR=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
REPO_ROOT=$(CDPATH='' cd -- "${SCRIPT_DIR}/../.." && pwd)
cd "${REPO_ROOT}"

TABLE="scripts/ci/feature-lanes.txt"
WORKFLOW=".github/workflows/ci.yml"

if ! command -v jq > /dev/null 2>&1; then
  echo "assert-feature-lanes: jq is required but not installed" >&2
  exit 1
fi

for required in "${TABLE}" "${WORKFLOW}"; do
  if [ ! -f "${required}" ]; then
    echo "assert-feature-lanes: ${required} is missing" >&2
    exit 1
  fi
done

WORK=$(mktemp -d)
trap 'rm -rf "${WORK}"' EXIT

# --- Ground truth: the features Cargo itself reports ------------------------
#
# `cargo metadata`, NOT a grep over Cargo.toml's [features] block. The manifest
# is not the whole story: an optional dependency named WITHOUT `dep:` also
# creates an implicit feature (here: `tinyagents`, via `tiny = ["tinyagents"]`),
# and a grep for `^name =` would miss it. A table that silently omits a feature
# is the exact hole this script exists to close, so the enumeration has to come
# from the resolver rather than from the text.
cargo metadata --no-deps --locked --format-version 1 \
  | jq -r '.packages[] | select(.name == "opencompany-core" or .name == "opencompany-tui") | if .name == "opencompany-core" then .features | keys[] else .features | keys[] | "opencompany-tui/" + . end' \
  | sort > "${WORK}/features"

if [ ! -s "${WORK}/features" ]; then
  echo "::error title=Feature enumeration matched nothing::cargo metadata reported no features for the opencompany package, so this check is green while asserting nothing. The package name or the manifest layout changed." >&2
  exit 1
fi

# --- The table --------------------------------------------------------------
# Strip comments and blank lines, trim each field, keep `feature|status|features|detail`.
#
# WHOLE-LINE comments only — `^[[:space:]]*#` rather than a bare `#`. An `owed`
# row's reason must name a tracking issue, and `#800` is a `#`: stripping
# inline would silently empty the one field that row is required to fill, and
# the check would then fail for a reason that is not the truth.
sed -e 's/^[[:space:]]*#.*$//' "${TABLE}" \
  | awk -F'|' 'NF >= 4 {
      for (i = 1; i <= 4; i++) { gsub(/^[ \t]+|[ \t]+$/, "", $i) }
      if ($1 != "") { print $1 "|" $2 "|" $3 "|" $4 }
    }' > "${WORK}/rows"

if [ ! -s "${WORK}/rows" ]; then
  echo "::error title=Classification table matched nothing::${TABLE} parsed to zero rows, so every feature would appear unclassified (or nothing would be checked at all). The file's format changed." >&2
  exit 1
fi

cut -d'|' -f1 "${WORK}/rows" | sort > "${WORK}/classified"

# A duplicate row is ambiguous — two answers for one feature, and the loop below
# would check whichever came first.
dupes=$(uniq -d < "${WORK}/classified" || true)
if [ -n "${dupes}" ]; then
  echo "::error title=Duplicate rows in the feature table::${TABLE} classifies these features more than once:" >&2
  echo "${dupes}" >&2
  exit 1
fi

failed=0

# --- Every feature is classified, and every row is a real feature -----------

unmapped=$(comm -23 "${WORK}/features" "${WORK}/classified")
if [ -n "${unmapped}" ]; then
  echo "::error title=Unclassified Cargo feature::A feature exists with no row in ${TABLE}, so nothing records whether any lane runs its tests. Add a row." >&2
  echo "${unmapped}" | sed 's/^/  /' >&2
  failed=1
fi

stale=$(comm -13 "${WORK}/features" "${WORK}/classified")
if [ -n "${stale}" ]; then
  echo "::error title=Stale row in the feature table::${TABLE} names features that no longer exist in Cargo.toml. Remove the rows." >&2
  echo "${stale}" | sed 's/^/  /' >&2
  failed=1
fi

# --- Gated-test detection ---------------------------------------------------
#
# Prints one `file:line: description` per gated TEST site for feature $1.
# Covers the three idioms named in the header.
gated_tests_for() {
  feature="$1"

  # (a) and (b): scan every Rust source for the two inline shapes.
  find crates/opencompany-core/src crates/opencompany-core/tests -name '*.rs' -type f 2>/dev/null | sort | while IFS= read -r file; do
    awk -v feat="${feature}" -v file="${file}" '
      # (a) a test module or test-only helper gated on the feature
      index($0, "cfg(all(test, feature = \"" feat "\"))") {
        printf "%s:%d: gated test module/helper: #[cfg(all(test, feature = \"%s\"))]\n", file, NR, feat
      }
      # (b) a plain feature cfg standing immediately above a test attribute
      index($0, "#[cfg(feature = \"" feat "\")]") { pending = NR; next }
      pending && (NR - pending) <= 3 && /#\[(tokio::)?test\]/ {
        printf "%s:%d: gated test: #[cfg(feature = \"%s\")] above %s\n", file, pending, feat, "#[test]"
        pending = 0
        next
      }
      pending && (NR - pending) > 3 { pending = 0 }
    ' "${file}"
  done

  # (c) a whole module gated on the feature, with the tests inside it. Resolve
  # `mod y;` to y.rs or y/mod.rs relative to the declaring file, then look for
  # test attributes in that module's own source.
  find crates/opencompany-core/src -name '*.rs' -type f 2>/dev/null | sort | while IFS= read -r file; do
    dir=$(dirname "${file}")
    awk -v feat="${feature}" '
      index($0, "#[cfg(feature = \"" feat "\")]") { pending = NR; next }
      pending && (NR - pending) <= 2 && match($0, /^[ \t]*(pub[ \t]+)?mod[ \t]+[A-Za-z0-9_]+[ \t]*;/) {
        line = $0
        sub(/^[ \t]*(pub[ \t]+)?mod[ \t]+/, "", line)
        sub(/[ \t]*;.*$/, "", line)
        print line
        pending = 0
        next
      }
      pending && (NR - pending) > 2 { pending = 0 }
    ' "${file}" | while IFS= read -r modname; do
      [ -n "${modname}" ] || continue
      for candidate in "${dir}/${modname}.rs" "${dir}/${modname}/mod.rs"; do
        [ -f "${candidate}" ] || continue
        moddir=$(dirname "${candidate}")
        if [ "$(basename "${candidate}")" = "mod.rs" ]; then
          scan=$(find "${moddir}" -name '*.rs' -type f 2>/dev/null)
        else
          scan="${candidate}"
        fi
        for target in ${scan}; do
          hit=$(grep -nE '#\[(tokio::)?test\]' "${target}" | head -n 1 || true)
          if [ -n "${hit}" ]; then
            echo "${target}:${hit%%:*}: gated module \`${modname}\` (gated in ${file}) contains tests"
          fi
        done
      done
    done
  done
}

# --- Per-row checks ---------------------------------------------------------

lanes_checked=0
compile_only_checked=0
owed_checked=0

while IFS='|' read -r feature status features detail; do
  [ -n "${feature}" ] || continue

  # Workspace-member features are identified in the table as
  # `package/feature`; source-level gated-test detection still receives the
  # Cargo feature name itself.
  source_feature=${feature#*/}

  case "${status}" in
    tested | partial)
      if [ -z "${features}" ] || [ "${features}" = "-" ]; then
        echo "::error title=Row names no feature set::\`${feature}\` is ${status} but its features column is empty. It must name the exact --features string the lane passes." >&2
        failed=1
        continue
      fi

      if [ "${features}" = "(default)" ]; then
        # The default set is not passed via --features; it is what a bare
        # `cargo test` builds. Assert that bare invocation exists.
        if ! grep -qE 'cargo test --locked[[:space:]]*$' "${WORKFLOW}"; then
          echo "::error title=No default test lane::\`${feature}\` is covered by the default feature set, but ${WORKFLOW} has no bare \`cargo test --locked\` line to run it." >&2
          failed=1
        fi
      # A lane is either a direct `cargo test … --features X` line or a
      # `run-scoped-suite.sh <label> X <filter>` invocation, which is the same
      # cargo call with the non-zero-count assertion wrapped around it. Both
      # forms are accepted deliberately: the wrapper is the preferred shape for
      # new lanes, and the pre-existing direct invocations are not worth churning
      # to satisfy a grep.
      elif ! grep -q -- "cargo test .*--features ${features}" "${WORKFLOW}" \
        && ! grep -qE "run-scoped-suite\.sh .*[[:space:]]${features}[[:space:]]" "${WORKFLOW}"; then
        echo "::error title=Feature has no lane::\`${feature}\` is classified ${status} on \`--features ${features}\`, but no \`cargo test\` line and no \`run-scoped-suite.sh\` invocation in ${WORKFLOW} enables that feature set. Add the lane, or reclassify the row." >&2
        failed=1
      fi

      if [ "${status}" = "partial" ]; then
        if [ -z "${detail}" ] || [ "${detail}" = "-" ]; then
          echo "::error title=Partial row names no filters::\`${feature}\` is partial, so its detail column must list the test filters its lane passes." >&2
          failed=1
        else
          for filter in ${detail}; do
            if ! grep -q -- "${filter}" "${WORKFLOW}"; then
              echo "::error title=Filter not in the workflow::\`${feature}\` claims filter \`${filter}\`, which appears nowhere in ${WORKFLOW}. The table and the lane disagree." >&2
              failed=1
            fi
          done
        fi
      fi

      lanes_checked=$((lanes_checked + 1))
      ;;

    compile-only)
      if [ -z "${detail}" ] || [ "${detail}" = "-" ]; then
        echo "::error title=Compile-only without a reason::\`${feature}\` is declared compile-only with no reason given. Say why no lane runs its tests — an unexplained compile-only is indistinguishable from an oversight, which is the whole failure this check exists to end." >&2
        failed=1
      fi

      found=$(gated_tests_for "${source_feature}")
      if [ -n "${found}" ]; then
        echo "::error title=Compile-only feature has gated tests::\`${feature}\` is declared compile-only in ${TABLE}, but these feature-gated tests exist. They are compiled by \`Check (--all-features)\` and RUN BY NOTHING." >&2
        echo "${found}" | sed 's/^/  /' >&2
        echo "  Fix the WIRING: add a lane to ${WORKFLOW} that runs them, then reclassify this row as tested/partial." >&2
        failed=1
      fi

      compile_only_checked=$((compile_only_checked + 1))
      ;;

    owed)
      # A lane is OWED: gated tests exist, they run nowhere, and the reason is
      # that the suite is currently RED rather than that anyone chose this.
      #
      # This status exists so a known-red gated suite is ENUMERABLE instead of
      # silent, which is the entire complaint of #770. It is deliberately the
      # least comfortable row in the table: it requires a tracking issue, and the
      # two checks below stop it becoming a place to park things.
      #
      #   * It must name an issue. "Owed" without a ticket is just silence with
      #     extra steps.
      #   * It must actually HAVE gated tests. A feature with none belongs in
      #     compile-only with a reason, so `owed` cannot be used to skip the
      #     harder question of why a feature has no tests at all.
      #
      # What it does NOT do is let a red lane sit in ci.yml. There is no lane —
      # that is the point. A lane permitted to be red is the state #428 was filed
      # about, at more expense.
      case "${detail}" in
        *'#'[0-9]*) : ;;
        *)
          echo "::error title=Owed lane without a tracking issue::\`${feature}\` is classified owed, but its detail names no issue (expected a \`#NNN\` reference). An owed lane with nowhere to read why is indistinguishable from the silence this table exists to end." >&2
          failed=1
          ;;
      esac

      if [ -z "$(gated_tests_for "${source_feature}")" ]; then
        echo "::error title=Owed lane has no gated tests::\`${feature}\` is classified owed, but no feature-gated test was found under it. A feature with no gated tests is compile-only — say so, with a reason, rather than recording a debt that does not exist." >&2
        failed=1
      fi

      owed_checked=$((owed_checked + 1))
      ;;

    *)
      echo "::error title=Unknown status::\`${feature}\` has status \`${status}\`; expected tested, partial, compile-only or owed." >&2
      failed=1
      ;;
  esac
done < "${WORK}/rows"

# --- The check checks itself ------------------------------------------------
#
# Issue #555's lesson, applied here: a guard whose pattern silently stops
# matching is green while asserting nothing, which is the same defect it was
# built to catch. Zero of either kind means the table parsed but the loop did
# not run, so say so rather than passing.
if [ "${lanes_checked}" -eq 0 ] || [ "${compile_only_checked}" -eq 0 ]; then
  echo "::error title=This check asserted nothing::Parsed $(wc -l < "${WORK}/rows" | tr -d ' ') rows but checked ${lanes_checked} lane(s) and ${compile_only_checked} compile-only row(s). A zero on either side means the status column stopped being read, so this script is green while checking nothing." >&2
  exit 1
fi

# The gated-test detector is the load-bearing half, so prove the detector itself
# still matches something rather than trusting that it does. `openhuman` gates
# tests by every idiom the detector knows; if it finds none there, the patterns
# have drifted away from the tree and every compile-only row above passed
# vacuously.
if [ -z "$(gated_tests_for openhuman)" ]; then
  echo "::error title=Gated-test detection matched nothing::The detector found no gated tests under \`openhuman\`, which gates many. Its patterns no longer match this tree, so every compile-only row passed without being checked." >&2
  exit 1
fi

echo
if [ "${failed}" -ne 0 ]; then
  cat >&2 << EOF
Feature-lane classification is out of date.

Every Cargo feature must say which CI lane runs its tests, or why none does.
The table is scripts/ci/feature-lanes.txt and its format is documented at the
top of that file.

Do not "fix" a failure by deleting a test's #[cfg] or by relaxing a filter to
match nothing. Both remove the code under test rather than running it, which is
the failure this check exists to name.
EOF
  exit 1
fi

echo "Feature lanes: $(wc -l < "${WORK}/features" | tr -d ' ') features — ${lanes_checked} with a lane, ${compile_only_checked} compile-only and verified to have no gated tests, ${owed_checked} owed a lane (tracked)."
