#!/bin/sh
# Run the desktop shell against a live console dev server.
#
# ## Why this is a script and not a `beforeDevCommand`
#
# A debug build of the shell loads `devUrl` (`http://localhost:5173`) rather
# than the embedded bundle, so without a dev server the window is blank. The
# obvious fix is `build.beforeDevCommand` in `src-tauri/tauri.conf.json`, and
# it is wrong: the Tauri CLI runs that hook from a directory it *derives* by
# scanning for a `package.json`, and which one it picks is not stable — on a
# macOS checkout it lands in `frontend/`, on CI's runner it landed in
# `vendor/openhuman/`. No relative path is correct from both, which is why
# those hooks are deliberately empty and why `ci.yml` packages from two
# different working directories to keep them that way (issue #616).
#
# A script has the one thing the hook does not: it knows where it is. Every
# path below is absolute, derived from `$0`, so nothing here depends on the
# directory it was invoked from.
set -eu

SCRIPT_DIR=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
REPO_ROOT=$(CDPATH='' cd -- "${SCRIPT_DIR}/.." && pwd)

# The port `devUrl` names. Not configurable here on purpose: a dev server on
# some other port is a window that loads nothing, and silently picking a
# different one is how that becomes a mystery instead of an error.
DEV_PORT=5173
DEV_URL="http://localhost:${DEV_PORT}"

usage() {
    cat >&2 <<'EOF'
Usage: ./scripts/desktop-dev.sh

Starts the console dev server (if one is not already up) and runs the desktop
shell against it. Ctrl-C stops both.

Environment:
  OPENCOMPANY_DATA_DIR   Instance data root. Point it at a scratch directory to
                         leave your installed application's data alone:

    OPENCOMPANY_DATA_DIR=$PWD/target/desktop-dev ./scripts/desktop-dev.sh

  DESKTOP_FEATURES       Cargo features for the shell. Defaults to the set the
                         release workflow ships, so what you run matches what
                         we ship. Set it empty to build the leaner default:

    DESKTOP_FEATURES= ./scripts/desktop-dev.sh
EOF
}

if [ "$#" -ne 0 ]; then
    usage
    exit 2
fi

# Whether *the console* is on the port — not merely whether something is.
#
# `-f` rejects a non-2xx answer and the marker rejects a 200 from somebody
# else's service. Both matter: the shell loads whatever is at `devUrl` without
# asking what it is, so reusing a stranger's port shows their page inside the
# OpenCompany window, and an error page would show as a blank one.
# `OpenCompany Console` is the `<title>` in `frontend/index.html`, which the
# dev server serves verbatim.
serving() {
    curl -fsS --max-time 2 "${DEV_URL}/" 2>/dev/null | grep -q 'OpenCompany Console'
}

DEV_SERVER_PID=""

cleanup() {
    # Only ever the server this script started. A dev server that was already
    # running belongs to whoever started it, and killing it would close
    # someone else's terminal out from under them.
    if [ -n "${DEV_SERVER_PID}" ]; then
        kill "${DEV_SERVER_PID}" 2>/dev/null || true
        wait "${DEV_SERVER_PID}" 2>/dev/null || true
    fi
}
trap cleanup EXIT INT TERM

if serving; then
    echo "desktop-dev: reusing the console dev server already on ${DEV_URL}"
else
    # Resolved here rather than up front: reusing a server someone else started
    # needs no package manager at all, and exiting for a missing `pnpm` on that
    # path would refuse a run that was about to work.
    #
    # `pnpm` first because `pnpm-workspace.yaml` declares it, `npm` second
    # because that is what CI installs with — either can run the `dev` script
    # whichever one populated `node_modules`.
    if command -v pnpm >/dev/null 2>&1; then
        PACKAGE_MANAGER=pnpm
    elif command -v npm >/dev/null 2>&1; then
        PACKAGE_MANAGER=npm
    else
        echo "desktop-dev: neither pnpm nor npm is on PATH" >&2
        exit 1
    fi

    if [ ! -d "${REPO_ROOT}/frontend/node_modules" ]; then
        echo "desktop-dev: install the console's dependencies first:" >&2
        # `cd` rather than a flag, which pnpm spells `--dir` and npm `--prefix`
        # — printing one manager's flag beside the other's name is a command
        # that does not run.
        echo "    cd '${REPO_ROOT}/frontend' && ${PACKAGE_MANAGER} install" >&2
        exit 1
    fi
    echo "desktop-dev: starting the console dev server on ${DEV_URL}"
    # `cd` in a subshell rather than a `--dir`/`--prefix` flag, which the two
    # package managers spell differently.
    (cd "${REPO_ROOT}/frontend" && exec "${PACKAGE_MANAGER}" run dev) &
    DEV_SERVER_PID=$!

    # Waited for rather than slept past: the shell loads `devUrl` the moment it
    # opens its window, and a race here is exactly the blank screen this script
    # exists to prevent.
    #
    # A wall-clock deadline, not an iteration count. Each `serving` call can
    # burn its full 2s timeout, so counting iterations meant "30 seconds" was
    # anywhere from 30 to 150 depending on how the failures happened to fall.
    deadline=$(($(date +%s) + 30))
    until serving; do
        if ! kill -0 "${DEV_SERVER_PID}" 2>/dev/null; then
            echo "desktop-dev: the console dev server exited" >&2
            exit 1
        fi
        if [ "$(date +%s)" -ge "${deadline}" ]; then
            echo "desktop-dev: the console did not answer on ${DEV_URL} within 30s" >&2
            exit 1
        fi
        sleep 0.5
    done
fi

# The feature set the SHIPPED desktop carries, read from the release workflow
# that declares it (issue #1823).
#
# Without this the desktop you RUN is not the desktop we SHIP, and it differs in
# exactly the surfaces the desktop exists to provide. `acp` gates
# `RuntimeBuilder::with_acp_agents`, so a locally-run desktop resolved every
# `transport = "local"` harness as unavailable — a `claude`-bound teammate could
# not take a turn, on the one host that has an `AcpAgentFactory` to give.
# `composio` gates whether the connector tiles are compiled at all, so they read
# "this build was compiled without", which is not an instruction a desktop user
# can follow.
#
# **Parsed rather than copied.** A second literal here is a third place to
# forget: the release workflow and `ci.yml` already carry the same string with a
# comment asking a human to keep them in step, and this script would have made
# that promise harder to keep at exactly the moment it started to matter. The
# workflow stays the source of truth; this reads it.
#
# Anchored on the key at the start of a line, so a mention of the name inside a
# comment cannot be picked up instead. A miss is fatal rather than a silent
# fall-back to no features: that would restore the very divergence this exists
# to close, and do it quietly.
RELEASE_WORKFLOW="${REPO_ROOT}/.github/workflows/release-desktop-macos.yml"
# An explicitly-set `DESKTOP_FEATURES` wins, even when empty — that is someone
# deliberately asking for the leaner build. Only an UNSET one is read from the
# workflow, and a read that finds nothing is fatal: falling back to no features
# would quietly restore the divergence this exists to close.
#
# The `+set` test has to happen BEFORE the assignment, because after it the
# variable is set either way and the two cases are indistinguishable.
if [ -z "${DESKTOP_FEATURES+set}" ]; then
    DESKTOP_FEATURES=$(
        sed -n 's/^[[:space:]]*DESKTOP_RELEASE_FEATURES:[[:space:]]*//p' \
            "${RELEASE_WORKFLOW}" | head -n 1
    )
    if [ -z "${DESKTOP_FEATURES}" ]; then
        echo "desktop-dev: could not read DESKTOP_RELEASE_FEATURES from ${RELEASE_WORKFLOW}" >&2
        echo "desktop-dev: set DESKTOP_FEATURES=... to choose the set yourself" >&2
        exit 1
    fi
fi
if [ -n "${DESKTOP_FEATURES}" ]; then
    echo "desktop-dev: building with --features ${DESKTOP_FEATURES}"
else
    echo "desktop-dev: building with no extra features (DESKTOP_FEATURES is empty)"
fi

# The CLI from `frontend/node_modules`, as `ci.yml` uses, falling back to a
# `cargo install`ed one. Run from `src-tauri` so the CLI finds this project:
# it searches *subfolders* of the working directory, so from `frontend/` it
# would pick the console wrapper in `frontend/src-tauri/` instead — a different
# application that happens to share this one's `productName`.
#
# `--features` is the CLI's own flag rather than the `-- --features` the release
# build uses. Both reach cargo, but they reach it differently: on `dev` a bare
# `--` means "arguments for the runner", and a reader has to already know the
# runner is cargo to see that the features land anywhere. `tauri build` has no
# such flag, which is why the workflow spells it the other way.
TAURI_CLI="${REPO_ROOT}/frontend/node_modules/.bin/tauri"
cd "${REPO_ROOT}/src-tauri"
if [ -n "${DESKTOP_FEATURES}" ]; then
    set -- --features "${DESKTOP_FEATURES}"
else
    set --
fi
if [ -x "${TAURI_CLI}" ]; then
    "${TAURI_CLI}" dev "$@"
else
    cargo tauri dev "$@"
fi
