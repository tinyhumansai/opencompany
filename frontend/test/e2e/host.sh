#!/usr/bin/env bash
#
# Brings up the OpenCompany host the end-to-end suite drives, so that running
# the suite is one command rather than an incantation nobody remembers.
#
# Playwright starts this as its `webServer` whenever `PW_BASE_URL` is NOT set
# (see ../../playwright.config.ts). Set `PW_BASE_URL` and this script is not
# used at all — a host you brought up yourself stays entirely your business.
#
# It deliberately does NOT build the Rust binary. `cargo build` on a cold target
# directory is minutes of silence, and a test harness that appears to hang is
# worse than one that tells you what to run. It DOES rebuild the console bundle,
# which takes about two seconds and is the difference between testing your
# working tree and testing whatever `dist/` happened to hold — a stale bundle is
# how a suite comes to report on code that is not the code under test.
#
# Env:
#   PW_HOST_BIND            address to bind         (default 127.0.0.1:<port derived
#                           from this checkout's path, so worktrees do not collide)
#   PW_HOST_COMPANY         company to load         (default companies/e2e_harness)
#   PW_HOST_DATA_DIR        instance data root      (default target/e2e/data; wiped
#                           each run ONLY while it stays inside target/e2e — see below)
#   PW_HOST_BINARY          path to the binary      (default target/debug/opencompany)
#   PW_HOST_PASSTHROUGH     space-separated extra variable names to forward to the host
#   PW_SKIP_CONSOLE_BUILD   set to 1 to reuse the existing frontend/dist

set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(cd "$here/../../.." && pwd)"

# Default port derived from this checkout's path, so two worktrees running the
# suite at once do not land on one port. Playwright normally passes PW_HOST_BIND
# in; this is the standalone path, and it computes the SAME number from the same
# path (first 4 bytes of sha256, mod 8800, offset 8100) so the two agree. See the
# `derivedPort` comment in ../../playwright.config.ts for why a fixed default is
# actively dangerous here rather than merely inconvenient.
derived_port=$(( 8100 + 16#$(printf '%s' "$root" | sha256sum | cut -c1-8) % 8800 ))
bind="${PW_HOST_BIND:-127.0.0.1:$derived_port}"
company="${PW_HOST_COMPANY:-$root/companies/e2e_harness}"
data_dir="${PW_HOST_DATA_DIR:-$root/target/e2e/data}"
binary="${PW_HOST_BINARY:-$root/target/debug/opencompany}"

if [[ ! -x "$binary" ]]; then
  cat >&2 <<EOF
[e2e host] No OpenCompany binary at:
    $binary

The suite drives the real host, so one has to be built first. From the
repository root:

    cargo build --locked --bin opencompany

That is the default feature set, which is all most of this suite needs — the
harness company boots on the offline echo brain. Four specs need an agent that
actually runs (issue #467); for those, build

    cargo build --locked --features openhuman,mcp --bin opencompany

and run the suite with 'npm run e2e:live'. Point PW_HOST_BINARY elsewhere to
use a release build or a binary you keep somewhere else.
EOF
  exit 1
fi

if [[ "${PW_SKIP_CONSOLE_BUILD:-}" != "1" ]]; then
  echo "[e2e host] building the console bundle (PW_SKIP_CONSOLE_BUILD=1 to skip)" >&2
  (
    cd "$root/frontend"
    npm run build >&2
    # The pages SDK is a separate Vite build. The main build clears `dist/`,
    # so it must be emitted afterward or the shell's import map resolves
    # `/pages-sdk/*.mjs` to the SPA fallback instead of a module.
    npm run build:pages-sdk >&2
  )
fi

if [[ ! -f "$root/frontend/dist/index.html" ]]; then
  echo "[e2e host] no console bundle at $root/frontend/dist — run 'npm run build' in frontend/." >&2
  exit 1
fi

# ---------------------------------------------------------------------------
# The instance data root, which this script DELETES.
# ---------------------------------------------------------------------------
# A fresh root per run: the specs create and delete their own workflows, but a
# run that failed half way leaves them behind, and the next run should not have
# to reason about what the last one left.
#
# Because it deletes, where PW_HOST_DATA_DIR is allowed to point is a safety
# question and not a convenience one. A value that is mistyped — or simply
# inherited from a shell that set it for something else — must not be able to
# take a home directory with it.
#
# The rule: a run only ever deletes INSIDE `target/e2e`, the repository's own
# scratch area. Both paths are canonicalised first (`cd` + `pwd -P`), so
# neither a `..` nor a symlink can walk out of it. Anywhere else is still
# usable — pointing the host at a data root you keep is a legitimate thing to
# want — it is simply reused as it stands, and the run says so rather than
# quietly removing it.
mkdir -p "$data_dir"
data_dir="$(cd "$data_dir" && pwd -P)"

scratch="$root/target/e2e"
mkdir -p "$scratch"
scratch="$(cd "$scratch" && pwd -P)"

# `"$scratch"/?*`, not `"$scratch"*`: a sibling `target/e2e-of-my-own` must not
# match, and neither must `$scratch` itself — playwright.config.ts writes the
# suite's storage state directly into it, and this runs before global-setup.
if [[ "$data_dir" == "$scratch"/?* ]]; then
  rm -rf -- "$data_dir"
  mkdir -p "$data_dir"
else
  echo "[e2e host] data root is not inside the repository's target/e2e scratch area;" >&2
  echo "[e2e host] reusing it as it stands rather than deleting it." >&2
fi

echo "[e2e host] serving $company on $bind (data: $data_dir)" >&2

# ---------------------------------------------------------------------------
# The host's environment, built up rather than inherited.
# ---------------------------------------------------------------------------
# `env VAR=…` keeps everything the caller already had, and several inherited
# variables do not fail the run — they change what the host IS, and the suite
# then fails somewhere else entirely. `OPENCOMPANY_PUBLIC_URL`, or any
# `OPENCOMPANY_MAIL_*` transport, stops the host echoing the magic-link
# `dev_code`; that code is the only way global-setup.ts can sign the suite in,
# so the run dies in bootstrap pointing at nothing. `OPENCOMPANY_STORAGE` and
# `OPENCOMPANY_TENANT_ID` are worse: they would move the host onto a different
# backend, and it would come up.
#
# So `env -i`, and name what goes in. A denylist has to grow every time another
# variable gains the power to break this, and is wrong in the meantime without
# anyone knowing. An allowlist can only be wrong by omission, which shows up as
# a host that will not start — loudly, once, on the person who can fix it.
#
# PW_HOST_PASSTHROUGH is the escape hatch that keeps the allowlist honest: a
# feature-gated binary supplied through PW_HOST_BINARY needs its inference
# credentials, and those arrive by environment.
host_env=(
  "OPENCOMPANY_CONSOLE_DIR=$root/frontend/dist"
  "OPENCOMPANY_DATA_DIR=$data_dir"
  # Issue #1844: this script always boots from a wiped data root (see above),
  # so without this every spec's very first navigation would hit the new
  # blocking onboarding gate — none of them know the three-step funnel exists.
  # Specs that DO want to see the gate drive it through `page.route()`
  # interception instead of a real activation state, so this is safe to set
  # unconditionally for both companies this script serves.
  "OPENCOMPANY_SKIP_ACTIVATION_GATE=1"
)

passthrough=(HOME PATH TMPDIR TZ LANG LC_ALL RUST_LOG RUST_BACKTRACE)
if [[ -n "${PW_HOST_PASSTHROUGH:-}" ]]; then
  # Deliberate word splitting: this variable is a list of variable NAMES.
  # shellcheck disable=SC2206
  passthrough+=(${PW_HOST_PASSTHROUGH})
fi

for name in "${passthrough[@]}"; do
  # `${!name+x}` is "set, even if empty" — an empty HOME is still a decision the
  # caller made, and dropping it would silently substitute a different one.
  if [[ -n "${!name+x}" ]]; then
    host_env+=("$name=${!name}")
  fi
done

# Loopback bind, and now a guaranteed-absent OPENCOMPANY_PUBLIC_URL: both
# load-bearing for the `dev_code` echo the sign-in depends on.
cd "$root"
exec env -i "${host_env[@]}" "$binary" serve --bind "$bind" --company "$company"
