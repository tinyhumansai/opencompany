#!/bin/sh
#
# Initialize the vendored openhuman crate's own submodules. Issue #592.
#
# THE ONE PLACE THIS RUNS. Do not re-inline these commands into a workflow.
#
# The list of crates is no longer written here — it is read from
# `vendor/openhuman/.gitmodules` at the pinned commit. Adding a submodule under
# `vendor/openhuman` needs no change to this file. See the derivation below for
# why a hardcoded copy could not survive a resync.
#
# This file exists because the block below was copied into four jobs of
# `ci.yml` and a fifth in `release.yml`, ~340 lines apart in a file long enough
# that a reader edits one copy and never learns the others exist. The failure
# that shape produces is the one issue #555 was filed for: a lane quietly not
# doing what its siblings do, found late and by accident. It had already
# happened by the time #592 was written — `release.yml` was missing
# `vendor/tinyhumans-sdk` and the nested `tinycortex` init entirely, and had
# gone unnoticed only because that workflow is `workflow_dispatch`-only and had
# never run.
#
# WHY THESE CRATES MUST BE ON DISK EVEN FOR A DEFAULT BUILD
#
# `openhuman_core` is an optional path dependency, but Cargo reads every path
# dependency's manifest during resolution regardless of feature selection. So
# the vendored crate's own path deps must resolve even for the default
# (offline) build where the `openhuman` feature is off — a missing checkout
# fails the DEFAULT build, not just `--features openhuman`.
#
# `tinyhumans-sdk` is the case that catches people out: it is NOT one of the
# `[patch]` targets. The vendored openhuman crate consumes it as a plain
# unconditional path dependency because it is unpublished (issue #499). Absent
# from this list, every lane breaks — which is exactly how `release.yml` came
# to be latently broken.
#
# `tinycortex` in turn declares its own `tinyagents` path dependency, so its
# manifest must resolve too. Harmless for the default build; required for any
# `--features openhuman` build.
#
# WHY SCOPED TO `vendor/` RATHER THAN `--recursive`
#
# A plain `--recursive` over `vendor/openhuman` takes every submodule openhuman
# declares, including desktop-only trees nothing in these builds compiles. The
# one that made this rule was `app/src-tauri/vendor/tauri-cef`, a heavy CEF
# checkout; openhuman replaced CEF with upstream Wry in `1843706c` and the
# submodule is gone, but the rule holds for whatever lands outside `vendor/`
# next. Scoping to the `vendor/` prefix excludes those structurally, without
# anyone having to name them.
#
# WHAT THIS DELIBERATELY DOES NOT DO
#
# It does not initialize the TOP-LEVEL submodules. Each caller chooses its own
# checkout strategy (`submodules: true` on `actions/checkout`, or an explicit
# init), and quietly changing that from in here would be a surprise. This
# script assumes `vendor/openhuman` is already checked out and only fills in
# the crates nested beneath it.
#
# Idempotent: safe to run repeatedly, and a no-op once the submodules are
# present at their pinned commits.
#
# Usage:
#   scripts/ci/init-vendored-submodules.sh
#
# Runs from any working directory — it resolves the repository root itself.

set -eu

SCRIPT_DIR=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
REPO_ROOT=$(CDPATH='' cd -- "${SCRIPT_DIR}/../.." && pwd)
cd "${REPO_ROOT}"

# Without this, git emits a bare "not a git repository" that says nothing about
# the actual mistake, which is a caller that never checked out the top level.
if [ ! -f vendor/openhuman/.gitmodules ]; then
  echo "init-vendored-submodules: vendor/openhuman/.gitmodules is missing, so" >&2
  echo "vendor/openhuman itself was never checked out. This script initializes" >&2
  echo "the crates NESTED under it and cannot create it." >&2
  echo >&2
  echo "In a workflow: pass \`submodules: true\` to actions/checkout." >&2
  echo "Locally: git submodule update --init vendor/openhuman" >&2
  exit 1
fi

# The list is READ FROM THE PIN, not written here.
#
# It used to be hardcoded, and that is the failure this now prevents: the list
# belongs to openhuman, so every resync of `vendor/openhuman` could invalidate
# it from the outside. Pin `cbc0b065` dropped `vendor/tinydocs` and
# `vendor/tinywallet`, and every Rust lane died on
# `error: pathspec 'vendor/tinydocs' did not match any file(s) known to git`
# before a single crate compiled. The symmetric failure — upstream ADDS a crate
# and the hardcoded list silently omits it — is the one that produced the
# `release.yml` breakage described above, and it is the worse of the two
# because nothing fails until something needs the missing manifest.
#
# The `vendor/` prefix is the whole filter, and it is a STRUCTURAL one: those are
# the crates Cargo must resolve manifests for. Anything openhuman vendors outside
# `vendor/` is not a path dependency of the crate we build.
#
# It is also what keeps the heavy desktop trees out, which is why this is not a
# plain `--recursive`. The case that rule was written for lived at
# `app/src-tauri/vendor/tauri-cef` — outside `vendor/`, so the prefix excluded it
# by construction. (openhuman has since replaced CEF with upstream Wry and the
# submodule is gone, but the rule stands for whatever lands there next.)
#
# Deliberately NOT filtering by name. A `grep -v cef` here would match by
# substring, so a future crate whose path merely contains those letters would be
# dropped without a word — the silent-omission failure this whole derivation
# exists to prevent, reintroduced by the guard meant to help.
VENDORED_CRATES=$(
  git -C vendor/openhuman config -f .gitmodules --get-regexp '^submodule\..*\.path$' |
    awk '{ print $2 }' |
    grep '^vendor/' ||
    true
)

# An empty list would make the `git submodule update` below init EVERY
# submodule — including the desktop-only `tauri-cef` tree this script exists to
# avoid dragging in. Refuse instead: an empty read means the `.gitmodules`
# shape changed, and guessing is how a lane starts doing something nobody asked
# for.
if [ -z "${VENDORED_CRATES}" ]; then
  echo "init-vendored-submodules: vendor/openhuman/.gitmodules declares no" >&2
  echo "submodule under vendor/. Either the pin is broken or the layout" >&2
  echo "changed; this script will not fall back to initializing everything." >&2
  exit 1
fi

# Unquoted on purpose — the newline-separated list becomes one argument per
# path. Submodule paths carry no whitespace.
# shellcheck disable=SC2086
git -C vendor/openhuman submodule update --init --depth 1 ${VENDORED_CRATES}

# SECOND LEVEL: each of the crates just checked out may itself declare
# `vendor/`-prefixed submodules that its own manifest needs (a path dependency
# one hop further down the tree). `tinycortex` declaring its own `tinyagents`
# path dependency was the first case of this; this used to be a single
# hardcoded `tinycortex` → `vendor/tinyagents` block.
#
# That hardcoding is exactly the failure this file's header describes:
# `tinyagents` later grew its own nested path dependency, `tinytools`
# (extracted from what used to live inline in `tinyagents`), vendored at
# `vendor/openhuman/vendor/tinyagents/vendor/tinytools`. The hardcoded block
# only ever looked at `tinycortex`, so `tinyagents`'s own manifest resolution
# broke with `failed to read .../tinytools/crates/tinytools/Cargo.toml — No
# such file or directory` — the crate was never cloned, because nothing here
# knew to. Generalizing to read each crate's own `.gitmodules`, the same way
# the top-level list above is read rather than written by hand, is what keeps
# the next one of these from happening the same way.
#
# Depth is deliberately capped at two levels (this loop does not recurse into
# what it just checked out). Nothing vendored today needs a third level, and
# recursing here would reopen the exact `--recursive` risk the top-level
# derivation is scoped to avoid: an unbounded walk into diamond-shared trees
# quietly initializing something a build does not need. When a third level is
# needed, extend this deliberately with the same reasoning, not by looping.
for crate in ${VENDORED_CRATES}; do
  crate_path="vendor/openhuman/${crate}"

  # Conditional per crate because the pin decides which of these crates are
  # vendored at all and which of those declare further nested submodules; a
  # resync dropping one must not fail this script.
  if [ ! -f "${crate_path}/.gitmodules" ]; then
    continue
  fi

  NESTED_CRATES=$(
    git -C "${crate_path}" config -f .gitmodules --get-regexp '^submodule\..*\.path$' |
      awk '{ print $2 }' |
      grep '^vendor/' ||
      true
  )

  # Unlike the top-level list, an empty result here is expected and fine — most
  # vendored crates declare no further nested submodules (or only a `wiki/`,
  # excluded by the same `vendor/` prefix filter as above). Only the top-level
  # read treats empty as an error, because there the alternative is silently
  # falling through to a plain `--recursive`.
  if [ -z "${NESTED_CRATES}" ]; then
    continue
  fi

  # shellcheck disable=SC2086
  git -C "${crate_path}" submodule update --init --depth 1 ${NESTED_CRATES}
done
