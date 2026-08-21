#!/usr/bin/env bash
#
# Fail if the tree holds more than one Tauri app, or if a script would launch
# the wrong one.
#
# The Tauri CLI locates a project by scanning SUBFOLDERS of its working
# directory, not ancestors. That makes "which app am I running" a property of
# where the command was typed, and it is invisible in the command itself.
#
# The tree had two: the shell in `src-tauri/`, and a leftover console wrapper in
# `frontend/src-tauri/` that shared its `productName`. Because `tauri:dev` and
# `tauri:build` live in `frontend/package.json`, and npm runs a script from its
# manifest's own directory, `npm run tauri:dev` — the obvious way to start the
# desktop app — started the wrapper. The wrapper registered one command,
# `desktop_config`, which the console had stopped invoking; every `oc_*` command
# the console uses to find a host was missing from its `generate_handler!`. The
# window opened, the console rendered, and no server was ever reachable. Nothing
# in CI could see it: both crates compiled, both were tested, and the lane
# packaged only the right one.
#
# Two rules, because either alone lets the failure back:
#
#   1. Exactly one `tauri.conf.json`, and it is `src-tauri/tauri.conf.json`. A
#      second app is the ambiguity itself; there is no version of it that is
#      safe just because it is currently correct.
#   2. No `package.json` script invokes the CLI without first establishing a
#      directory. An unqualified `tauri` (or `cargo tauri`, or `npx tauri`)
#      resolves its project from npm's working directory, which is wherever the
#      manifest happens to live — the mechanism above. A `cd` earlier in the
#      same script, or a path-qualified binary, both say which app is meant.
set -uo pipefail

cd "$(dirname "$0")/../.." || exit 1

status=0

# Vendored checkouts own their apps; this rule is about this repository's.
prune=(
    -not -path './node_modules/*' -not -path '*/node_modules/*'
    -not -path './vendor/*'
    -not -path './target/*' -not -path '*/target/*'
    -not -path './worktrees/*'
    -not -path './.git/*'
)

configs=$(find . -name tauri.conf.json "${prune[@]}" | sed 's|^\./||' | sort)

if [ "${configs}" != "src-tauri/tauri.conf.json" ]; then
    echo "assert-single-tauri-app: expected exactly one Tauri app, at src-tauri/tauri.conf.json." >&2
    echo "Found:" >&2
    echo "${configs}" | sed 's/^/    /' >&2
    echo >&2
    echo "The CLI picks a project by scanning subfolders of the working directory," >&2
    echo "so a second app makes 'which app ran' depend on where the command was typed." >&2
    status=1
fi

# Whether one script value invokes the CLI with no directory established.
#
# Not "does the value start with tauri": `npm run build && tauri build` is the
# same bug one operator away, and it is the shape a hurried fix reaches for. So
# the value is split into command segments on `&&`, `||`, `;` and `|`, and the
# FIRST WORD of each segment is what gets judged — which also means a
# path-qualified `../frontend/node_modules/.bin/tauri` is not a bare `tauri` and
# a package name like `@tauri-apps/cli` is not a command at all.
#
# A `cd` in an earlier segment clears the rest of the value: the whole point of
# `cd ../src-tauri && … tauri build` is that it names the app, and a rule that
# rejected it would have nothing left to recommend.
unqualified_tauri() {
    local value=$1
    local segment first second saw_cd=0
    local segments=()

    # Collected line by line rather than with `for segment in $(...)` under
    # `IFS=$'\n'`. That reads well and is wrong: the word split below would then
    # not split on spaces either, so every segment's "first word" would be the
    # whole segment and the check would accept everything. `mapfile` would say
    # it in one line and is bash 4 — macOS ships 3.2, and a guard a developer
    # cannot run locally is a guard they find out about from CI.
    while IFS= read -r segment; do
        segments+=("${segment}")
    done < <(printf '%s\n' "${value}" | sed 's/&&/\n/g; s/||/\n/g; s/;/\n/g; s/|/\n/g')

    for segment in ${segments[@]+"${segments[@]}"}; do
        # shellcheck disable=SC2086 # deliberate: split the segment into words.
        set -- ${segment}
        first=${1-}
        second=${2-}
        case "${first}" in
            cd) saw_cd=1 ;;
            tauri | npx | pnpx)
                if [ "${first}" = "tauri" ] || [ "${second}" = "tauri" ]; then
                    [ "${saw_cd}" -eq 0 ] && return 0
                fi
                ;;
            cargo)
                if [ "${second}" = "tauri" ]; then
                    [ "${saw_cd}" -eq 0 ] && return 0
                fi
                ;;
        esac
    done
    return 1
}

while IFS= read -r manifest; do
    offenders=""
    # Every `"key": "value"` pair in the manifest. Dependency entries reach this
    # too and are harmless: a version range has no `tauri` command in it.
    while IFS= read -r pair; do
        key=${pair%%\"*}
        value=${pair#*\"}
        if unqualified_tauri "${value}"; then
            offenders="${offenders}    \"${key}\": \"${value}\""$'\n'
        fi
    done < <(
        grep -oE '"[^"]+"[[:space:]]*:[[:space:]]*"[^"]*"' "${manifest}" |
            sed -E 's/^"([^"]+)"[[:space:]]*:[[:space:]]*"(.*)"$/\1"\2/'
    )

    if [ -n "${offenders}" ]; then
        echo "assert-single-tauri-app: ${manifest} invokes the Tauri CLI with no directory established:" >&2
        printf '%s' "${offenders}" >&2
        echo >&2
        echo "npm runs a script from the manifest's own directory, so an unqualified" >&2
        echo "'tauri' picks up whatever project sits beneath it. Name the directory:" >&2
        echo '    "tauri:build": "npm run build && cd ../src-tauri && ../frontend/node_modules/.bin/tauri build"' >&2
        status=1
    fi
done < <(find . -name package.json "${prune[@]}")

if [ "${status}" -eq 0 ]; then
    echo "assert-single-tauri-app: one Tauri app (src-tauri/), no unqualified CLI invocations."
fi

exit "${status}"
