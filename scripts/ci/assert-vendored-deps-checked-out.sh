#!/bin/sh
#
# Assert that every vendored path dependency is checked out by every workflow
# job that builds. Issue #592, second instance.
#
# THE FAILURE THIS PREVENTS
#
# Cargo reads every path dependency's manifest during resolution regardless of
# feature selection, so a vendored crate missing from disk fails the DEFAULT
# build — not just the feature that uses it. Most jobs get this for free with
# `submodules: true` or `submodules: recursive`. `deploy-staging.yml` does not:
# it sets `submodules: false` on purpose (recursing would drag in openhuman's
# desktop-only tauri-cef checkout, which no server build compiles) and then
# names the submodules it needs by hand.
#
# A hand-written list rots. Adding a vendored path dependency without adding it
# to that list produces a build that is green on every pull request and fails
# only on deploy, because that workflow does not run on pull requests. This
# script is the check that turns that into a failing PR instead.
#
# WHAT IT CHECKS
#
# For each `path = "vendor/<name>/..."` dependency in Cargo.toml, every workflow
# that sets `submodules: false` must name `vendor/<name>` somewhere. Nested
# submodules *inside* a vendored crate are out of scope — those belong to
# `init-vendored-submodules.sh`, which derives them from the pinned
# `.gitmodules` rather than hardcoding them.
set -eu

cd "$(dirname "$0")/../.."

status=0

vendored="$(grep -Eo 'path = "vendor/[^/"]+' Cargo.toml \
  | sed 's|path = "vendor/||' \
  | sort -u)"

if [ -z "$vendored" ]; then
  echo "assert-vendored-deps: no vendored path dependencies found in Cargo.toml" >&2
  echo "If that is now true, delete this script; if it is not, the grep has rotted." >&2
  exit 1
fi

for workflow in .github/workflows/*.yml; do
  # Only workflows that opt out of automatic submodule checkout have to name
  # what they need. Everything else gets it from `submodules: true|recursive`.
  grep -q 'submodules: false' "$workflow" || continue

  for name in $vendored; do
    if ! grep -q "vendor/$name" "$workflow"; then
      echo "$workflow sets 'submodules: false' but never checks out vendor/$name" >&2
      status=1
    fi
  done
done

if [ "$status" -ne 0 ]; then
  echo >&2
  echo "Cargo reads every path dependency's manifest during resolution, so a" >&2
  echo "missing vendored checkout fails the default build. Add the submodule to" >&2
  echo "that workflow's 'git submodule update --init' line." >&2
  exit 1
fi

echo "assert-vendored-deps: every vendored path dependency is checked out"
echo "  vendored: $(echo "$vendored" | tr '\n' ' ')"
