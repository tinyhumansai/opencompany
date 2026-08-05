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
#   PW_HOST_BIND            address to bind         (default 127.0.0.1:8080)
#   PW_HOST_COMPANY         company to load         (default companies/e2e_harness)
#   PW_HOST_DATA_DIR        instance data root      (default target/e2e/data, wiped each run)
#   PW_HOST_BINARY          path to the binary      (default target/debug/opencompany)
#   PW_SKIP_CONSOLE_BUILD   set to 1 to reuse the existing frontend/dist

set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(cd "$here/../../.." && pwd)"

bind="${PW_HOST_BIND:-127.0.0.1:8080}"
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

That is the default feature set, which is all this suite needs — the harness
company boots on the offline echo brain. Point PW_HOST_BINARY elsewhere to use
a release build or a feature-gated one.
EOF
  exit 1
fi

if [[ "${PW_SKIP_CONSOLE_BUILD:-}" != "1" ]]; then
  echo "[e2e host] building the console bundle (PW_SKIP_CONSOLE_BUILD=1 to skip)" >&2
  ( cd "$root/frontend" && npm run build >&2 )
fi

if [[ ! -f "$root/frontend/dist/index.html" ]]; then
  echo "[e2e host] no console bundle at $root/frontend/dist — run 'npm run build' in frontend/." >&2
  exit 1
fi

# A fresh instance root per run. The specs create and delete their own
# workflows, but a run that failed half way leaves them behind, and the next run
# should not have to reason about what the last one left.
rm -rf "$data_dir"
mkdir -p "$data_dir"

echo "[e2e host] serving $company on $bind (data: $data_dir)" >&2

# Loopback bind and no OPENCOMPANY_PUBLIC_URL, both load-bearing: the host only
# echoes a magic-link `dev_code` when it does not look routable, and
# global-setup.ts signs the whole suite in with that code.
cd "$root"
exec env \
  OPENCOMPANY_CONSOLE_DIR="$root/frontend/dist" \
  OPENCOMPANY_DATA_DIR="$data_dir" \
  "$binary" serve --bind "$bind" --company "$company"
