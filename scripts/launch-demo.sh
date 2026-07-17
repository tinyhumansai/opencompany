#!/bin/sh
# Launch or destroy one local OpenCompany demo stack.
set -eu

SCRIPT_DIR=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
REPO_ROOT=$(CDPATH='' cd -- "${SCRIPT_DIR}/.." && pwd)

# shellcheck source=scripts/lib/demos.sh
. "${SCRIPT_DIR}/lib/demos.sh"

usage() {
    cat >&2 <<'EOF'
Usage: ./scripts/launch-demo.sh <site> <up|down> [-v]

Examples:
  ./scripts/launch-demo.sh marketing up
  ./scripts/launch-demo.sh marketing down
  ./scripts/launch-demo.sh marketing down -v  # also delete its data volume
  ./scripts/launch-demo.sh agentic_software_company up

Run ./scripts/list-demos.sh to see available names.
EOF
}

if [ "$#" -lt 2 ] || [ "$#" -gt 3 ]; then
    usage
    exit 2
fi

requested_site=$1
action=$2
volume_flag=${3:-}

case "$requested_site" in
    '' | *[!a-zA-Z0-9_-]*)
        echo "opencompany: invalid demo name '${requested_site}'" >&2
        exit 2
        ;;
esac

company=$(resolve_demo_company "$requested_site")
company_dir="${REPO_ROOT}/companies/${company}"

if [ ! -f "${company_dir}/company.toml" ] && [ ! -f "${company_dir}/agents.toml" ]; then
    echo "opencompany: unknown demo '${requested_site}'" >&2
    echo "Run ./scripts/list-demos.sh to see available names." >&2
    exit 2
fi

case "$action" in
    up | down) ;;
    *)
        echo "opencompany: action must be 'up' or 'down', got '${action}'" >&2
        usage
        exit 2
        ;;
esac

if [ -n "$volume_flag" ] && { [ "$action" != "down" ] || [ "$volume_flag" != "-v" ]; }; then
    echo "opencompany: '-v' is only supported after 'down'" >&2
    usage
    exit 2
fi

if ! command -v docker >/dev/null 2>&1; then
    echo "opencompany: docker is required" >&2
    exit 127
fi

project=$(demo_project_name "$company")
compose_file="${REPO_ROOT}/docker-compose.yml"
dev_compose_file="${REPO_ROOT}/docker-compose.dev.yml"

echo "opencompany: ${action} '${company}' (Compose project: ${project})"

if [ "$action" = "up" ]; then
    # Intentionally attached: Ctrl-C stops the stack and returns to the shell.
    OPENCOMPANY_COMPANY="$company" docker compose \
        --project-directory "$REPO_ROOT" \
        --project-name "$project" \
        --file "$compose_file" \
        --file "$dev_compose_file" \
        up --build
else
    if [ "$volume_flag" = "-v" ]; then
        OPENCOMPANY_COMPANY="$company" docker compose \
            --project-directory "$REPO_ROOT" \
            --project-name "$project" \
            --file "$compose_file" \
            --file "$dev_compose_file" \
            down --remove-orphans --volumes
    else
        OPENCOMPANY_COMPANY="$company" docker compose \
            --project-directory "$REPO_ROOT" \
            --project-name "$project" \
            --file "$compose_file" \
            --file "$dev_compose_file" \
            down --remove-orphans
    fi
fi
