#!/bin/sh
# Focused tests for the Compose launcher without starting Docker.
set -eu

SCRIPT_DIR=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
REPO_ROOT=$(CDPATH='' cd -- "${SCRIPT_DIR}/.." && pwd)
TMP_DIR=$(mktemp -d)
trap 'rm -rf "$TMP_DIR"' EXIT HUP INT TERM

cat >"${TMP_DIR}/docker" <<'EOF'
#!/bin/sh
printf 'company=%s\n' "$OPENCOMPANY_COMPANY"
printf 'args=%s\n' "$*"
EOF
chmod +x "${TMP_DIR}/docker"

cat >"${TMP_DIR}/cargo" <<'EOF'
#!/bin/sh
printf 'cargo_args=%s\n' "$*"
EOF
chmod +x "${TMP_DIR}/cargo"

run_launcher() {
    PATH="${TMP_DIR}:$PATH" "${SCRIPT_DIR}/launch-demo.sh" "$@"
}

up_output=$(run_launcher marketing up)
printf '%s\n' "$up_output" | grep -F "company=agentic_marketing_agency" >/dev/null
printf '%s\n' "$up_output" | grep -F -- "--project-name opencompany-agentic-marketing-agency" >/dev/null
printf '%s\n' "$up_output" | grep -F -- "--file ${REPO_ROOT}/docker-compose.dev.yml" >/dev/null
printf '%s\n' "$up_output" | grep -F "up --build" >/dev/null
if printf '%s\n' "$up_output" | grep -F -- " -d" >/dev/null; then
    echo "launch-demo test: up unexpectedly runs detached" >&2
    exit 1
fi

down_output=$(run_launcher agentic_software_company down)
printf '%s\n' "$down_output" | grep -F "company=agentic_software_company" >/dev/null
printf '%s\n' "$down_output" | grep -F "down --remove-orphans" >/dev/null
if printf '%s\n' "$down_output" | grep -F -- "--volumes" >/dev/null; then
    echo "launch-demo test: plain down unexpectedly removes volumes" >&2
    exit 1
fi

down_volumes_output=$(run_launcher marketing down -v)
printf '%s\n' "$down_volumes_output" | grep -F "down --remove-orphans --volumes" >/dev/null

if run_launcher not-a-company up >/dev/null 2>&1; then
    echo "launch-demo test: unknown company unexpectedly succeeded" >&2
    exit 1
fi

if run_launcher marketing restart >/dev/null 2>&1; then
    echo "launch-demo test: unsupported action unexpectedly succeeded" >&2
    exit 1
fi

if run_launcher marketing up -v >/dev/null 2>&1; then
    echo "launch-demo test: up unexpectedly accepted -v" >&2
    exit 1
fi

backend_output=$(PATH="${TMP_DIR}:$PATH" \
    OPENCOMPANY_COMPANY=agentic_marketing_agency \
    OPENCOMPANY_FEATURES="sqlite tiny" \
    "${SCRIPT_DIR}/run-demo-backend.sh")
printf '%s\n' "$backend_output" | grep -F \
    "cargo_args=run --bin opencompany --features sqlite tiny -- serve --company companies/agentic_marketing_agency" \
    >/dev/null

echo "launch-demo tests passed"
