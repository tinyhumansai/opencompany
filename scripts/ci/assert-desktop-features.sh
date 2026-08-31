#!/usr/bin/env bash
#
# Fail if the desktop's shipped cargo feature set is not the one CI compiles and
# not the one `scripts/desktop-dev.sh` runs.
#
# Issue #1738. `src-tauri/Cargo.toml`'s feature list is NOT what the desktop
# ships. `acp` and `composio` are passed on the `tauri` command line — both are
# `= ["openhuman"]` in the root manifest, pure `cfg` switches adding no package,
# so a release can turn them on and still build `--locked`; the argument is on
# the `DESKTOP_RELEASE_FEATURES` env block in `release-desktop-macos.yml`.
#
# The cost of that is three copies of one string, and until #1738 there were
# only two: the dev script ran `tauri dev` bare. So a developer's desktop was
# the DEFAULT feature set and every DMG was the release one, which made them
# different products with nothing saying so. The visible half was Connections
# reporting `in_build: false`: eight tiles reading "not available here" over a
# card asking for a Composio token, on a build nobody ships.
# #1738 was filed reading that as the product's intent, which is what a surface
# only developers see and only users don't will keep producing.
#
# `ci.yml`'s copy already had a comment telling the next person to keep it in
# step with the release workflow. This is that instruction, enforced. The
# release workflow is the source of truth: it is the one whose value reaches a
# user.
#
# To change the shipped set: edit `DESKTOP_RELEASE_FEATURES` in
# `release-desktop-macos.yml`, run this script, and fix whatever it names.
set -euo pipefail

cd "$(dirname "$0")/../.."

RELEASE_WORKFLOW=.github/workflows/release-desktop-macos.yml
CI_WORKFLOW=.github/workflows/ci.yml
DEV_SCRIPT=scripts/desktop-dev.sh
CONSOLE_MANIFEST=frontend/package.json
DESKTOP_DOC=docs/spec/runtime/desktop.md

# Comma-separated cargo feature lists are order-insensitive to cargo but not to
# `=`, and a reordering is not a drift. Normalise before comparing so this
# script fails on the thing that matters — a feature present in one place and
# absent from another — rather than on a rewrite of the same set.
# Empty input must come back empty, not abort: a call site passing NO features is
# the failure this script is for, and `check` renders it `<none>`. `sed '/^$/d'`
# rather than `grep -v '^$'` because grep exits 1 when it emits nothing, which
# under `set -e -o pipefail` killed the script mid-run — it exited 1 without ever
# naming the offending call site, which is the one thing a reader needs.
normalise() {
  printf '%s' "$1" | tr ',' '\n' | sed 's/^[[:space:]]*//; s/[[:space:]]*$//' \
    | LC_ALL=C sort -u | sed '/^$/d' | paste -sd, -
}

expected_raw="$(
  sed -n 's/^[[:space:]]*DESKTOP_RELEASE_FEATURES:[[:space:]]*\(.*\)$/\1/p' \
    "$RELEASE_WORKFLOW" | head -1 | sed 's/#.*//; s/[[:space:]]*$//; s/^"//; s/"$//'
)"

if [ -z "$expected_raw" ]; then
  echo "assert-desktop-features: could not read DESKTOP_RELEASE_FEATURES from $RELEASE_WORKFLOW" >&2
  echo "  Either the release build stopped naming its features there, or this" >&2
  echo "  script's pattern is stale. Both need a human." >&2
  exit 1
fi

expected="$(normalise "$expected_raw")"
echo "$RELEASE_WORKFLOW ships: $expected"

# ...and the release build must actually PASS it. Everything below compares
# against this value on the strength of it being what reaches a user, so a
# `tauri build` that stopped consuming the variable would turn the source of
# truth into a lie: the DMG would revert to the default set while CI and the dev
# script kept `acp` and `composio`, and this script would report success. That is
# #1738 again with the roles reversed, and it is the one drift no comparison here
# could see. Symmetric with the `tauri dev` check further down.
# The command itself, not "some line in this file". Assembled by following
# backslash continuations from the `tauri build` line, because the invocation is
# wrapped across three of them and the features live on the last.
#
# Grepping the whole file was the first attempt and it was hollow: it collected
# every line mentioning `tauri build` OR `--features` and passed if ANY of them
# named the variable. Deleting the flag from the real build while any unrelated
# line still mentioned `DESKTOP_RELEASE_FEATURES` — an `echo`, a comment that
# survived the filter — left this green while the DMG reverted to the default
# set. The check has to bind the variable to the command that ships.
release_build="$(
  awk '
    /tauri build/ { collecting = 1 }
    collecting     { command = command " " $0 }
    collecting && !/\\[[:space:]]*$/ { print command; exit }
  ' "$RELEASE_WORKFLOW"
)"
if [ -z "$release_build" ]; then
  echo "assert-desktop-features: no 'tauri build' command found in $RELEASE_WORKFLOW." >&2
  echo "  Either the release stopped packaging with the Tauri CLI, or this" >&2
  echo "  script's pattern is stale. Both need a human." >&2
  exit 1
fi
if ! printf '%s' "$release_build" | grep -q 'DESKTOP_RELEASE_FEATURES'; then
  echo "assert-desktop-features: $RELEASE_WORKFLOW declares DESKTOP_RELEASE_FEATURES" >&2
  echo "  but its 'tauri build' does not pass it, so the shipped DMG would carry" >&2
  echo "  the default feature set while every other call site carries the release" >&2
  echo "  one. The command found was:" >&2
  printf '    %s\n' "$release_build" >&2
  exit 1
fi

status=0
found=0

# One `check <where> <value>` per call site. `$value` may be empty — that is
# the #1738 failure itself (a call site passing no features at all), so it must
# report rather than be skipped.
check() {
  local where="$1" actual_raw="$2" actual
  found=$((found + 1))
  actual="$(normalise "$actual_raw")"
  if [ "$actual" = "$expected" ]; then
    echo "  ok  $where -> $actual"
  else
    echo "  BAD $where -> '${actual:-<none>}' (release ships '$expected')" >&2
    status=1
  fi
}

# `ci.yml`'s Desktop lane: every cargo command run against the desktop manifest.
# Both the clippy and the test step must carry the features — a lane that lints
# the shipped set but tests the default one is the same hole in half.
#
# Selected by the CARGO COMMAND, never by the presence of `--features`. Filtering
# on the flag was a hole big enough to drive the original bug through: a step
# that dropped `--features` would not match, would therefore not be checked, and
# the script would report the remaining step as "ok" and exit 0. The one thing
# this exists to catch — a call site compiling the default set — was the one
# thing it skipped.
ci_lines="$(grep -nE 'cargo (clippy|test|check|build)[^|]*--manifest-path src-tauri/Cargo.toml' "$CI_WORKFLOW" || true)"
if [ -z "$ci_lines" ]; then
  echo "assert-desktop-features: no cargo command against src-tauri/Cargo.toml in $CI_WORKFLOW." >&2
  echo "  Either the Desktop lane stopped compiling the shell, or this script's" >&2
  echo "  pattern is stale. Both need a human." >&2
  exit 1
fi
while IFS= read -r entry; do
  line="${entry%%:*}"
  # No match leaves this empty, which `check` reports as `<none>` rather than
  # skipping. That is the point of selecting by command above.
  value="$(printf '%s' "${entry#*:}" | sed -n 's/.*--features[= ]\{1,\}\([^ ]*\).*/\1/p')"
  check "$CI_WORKFLOW:$line" "$value"
done <<< "$ci_lines"

# The dev launcher (`scripts/desktop-dev.sh`).
#
# It is checked DIFFERENTLY from every other call site, because #1823 gave it a
# better shape than a copy: it *parses* `DESKTOP_RELEASE_FEATURES` out of the
# release workflow at run time, with a `DESKTOP_FEATURES` override for someone
# who deliberately wants the leaner build. There is no literal to compare, and
# that is the point — a value derived from the source of truth cannot drift from
# it, so demanding a copy here would make the script worse.
#
# So the invariant is different in kind: it must DERIVE rather than duplicate,
# and what it derives must reach cargo.
# EXECUTABLE lines only. The first version grepped the whole file, and this
# script's own subject documents itself at length — `desktop-dev.sh` explains the
# derivation in a comment block naming both `DESKTOP_RELEASE_FEATURES` and the
# workflow. So deleting the `sed` that actually extracts the value left the
# comments behind, and the comments alone satisfied the check: green, while every
# dev build ran with no features at all. A check a comment can satisfy is a
# check about documentation, not behaviour.
dev_code="$(grep -vE '^[[:space:]]*#' "$DEV_SCRIPT")"
dev_names_workflow=0
dev_extracts_key=0
printf '%s' "$dev_code" | grep -q 'release-desktop-macos.yml' && dev_names_workflow=1
# The extraction itself: executable code that pulls the key out of something.
printf '%s' "$dev_code" | grep -qE '(sed|awk|grep|rg)[^|]*DESKTOP_RELEASE_FEATURES' && dev_extracts_key=1
if [ "$dev_names_workflow" -eq 1 ] && [ "$dev_extracts_key" -eq 1 ]; then
  echo "  ok  $DEV_SCRIPT derives the features from $RELEASE_WORKFLOW"
else
  echo "  BAD $DEV_SCRIPT no longer extracts DESKTOP_RELEASE_FEATURES from the release workflow in code" >&2
  [ "$dev_names_workflow" -eq 1 ] || echo "        (no executable line names release-desktop-macos.yml)" >&2
  [ "$dev_extracts_key" -eq 1 ] || echo "        (no executable line extracts DESKTOP_RELEASE_FEATURES)" >&2
  status=1
fi

# A re-introduced literal is a regression even if it happens to be correct
# today: it is a fourth copy of a string three other places already carry, and
# the whole reason this script exists is that such copies drift. Comments may
# name the features; a shell assignment may not.
dev_literal="$(
  grep -vE '^[[:space:]]*#' "$DEV_SCRIPT" | grep -nE '=[^|]*opencompany/[a-z-]+' || true
)"
if [ -n "$dev_literal" ]; then
  echo "  BAD $DEV_SCRIPT hardcodes a feature literal instead of reading the workflow:" >&2
  printf '        %s\n' "$dev_literal" >&2
  status=1
fi

# ...and the derived value must actually reach the build. `desktop-dev.sh`
# builds its argument list with `set -- --features "$DESKTOP_FEATURES"` and
# forwards `"$@"`, so a bare `tauri dev` with no `"$@"` is the regression to
# catch — that is precisely the shape #1738 was filed about.
dev_invocations="$(
  grep -vE '^[[:space:]]*#' "$DEV_SCRIPT" | grep -nE '(\$\{TAURI_CLI\}"?|cargo tauri)[[:space:]]+dev([[:space:]]|$)' || true
)"
if [ -z "$dev_invocations" ]; then
  echo "  BAD $DEV_SCRIPT -> no 'tauri dev' invocation found; this check asserts nothing" >&2
  status=1
else
  while IFS= read -r invocation; do
    text="${invocation#*:}"
    if printf '%s' "$text" | grep -qE '(--features|"\$@")'; then
      echo "  ok  $DEV_SCRIPT passes the features on:$(printf '%s' "$text" | sed 's/^[[:space:]]*/ /')"
    else
      echo "  BAD $DEV_SCRIPT launches WITHOUT the features:$(printf '%s' "$text" | sed 's/^[[:space:]]*/ /')" >&2
      status=1
    fi
  done <<< "$dev_invocations"
fi

# The developer PACKAGING path. `npm run tauri:build` is how a developer builds
# the app locally, and the command `docs/spec/runtime/desktop.md` documents does
# the same thing by hand.
#
# This is the entry point #1738 did not reach and the dev-launcher fix did not
# cover. A `tauri build` with no `--features` packages the DEFAULT set, so the
# artifact has Composio and ACP compiled out while looking in every other respect
# like the shipped app — worse than the dev-window version of the bug, because
# there is no dev server or console banner to hint that this is not the real
# thing. Someone reproducing a user report against a locally-packaged build would
# be testing a different product and have no way to know.
#
# CI's own `Package` steps (`tauri build --debug --no-bundle`) are deliberately
# NOT checked here. They exist to prove `tauri.conf.json` executes from two
# working directories (issue #616), not to compile a feature set, and the
# `Clippy`/`Test` steps above them already build the shipped one.
manifest_build="$(
  grep -oE '"tauri:build"[[:space:]]*:[[:space:]]*"[^"]*"' "$CONSOLE_MANIFEST" || true
)"
if [ -z "$manifest_build" ]; then
  echo "assert-desktop-features: no 'tauri:build' script in $CONSOLE_MANIFEST." >&2
  echo "  Either the console stopped offering a packaging script, or this" >&2
  echo "  script's pattern is stale. Both need a human." >&2
  exit 1
fi
manifest_features="$(printf '%s' "$manifest_build" | sed -n 's/.*--features[= ]\{1,\}\([^ "]*\).*/\1/p')"
check "$CONSOLE_MANIFEST (tauri:build)" "$manifest_features"

# The documented by-hand equivalent, which a developer is at least as likely to
# copy as to run the npm script.
doc_build="$(grep -nE '^cargo tauri build' "$DESKTOP_DOC" || true)"
if [ -z "$doc_build" ]; then
  echo "  BAD $DESKTOP_DOC -> documents no 'cargo tauri build' command to check" >&2
  status=1
else
  while IFS= read -r line; do
    value="$(printf '%s' "${line#*:}" | sed -n 's/.*--features[= ]\{1,\}\([^ ]*\).*/\1/p')"
    check "$DESKTOP_DOC:${line%%:*}" "$value"
  done <<< "$doc_build"
fi

if [ "$status" -ne 0 ]; then
  echo >&2
  echo "assert-desktop-features: bring every call site to '$expected', or change" >&2
  echo "  DESKTOP_RELEASE_FEATURES in $RELEASE_WORKFLOW if the release is the one" >&2
  echo "  that is wrong. A desktop developers run and a desktop users install" >&2
  echo "  must be the same build (issue #1738)." >&2
  exit 1
fi

echo "All $found desktop feature call site(s) agree with $RELEASE_WORKFLOW."
