#!/bin/sh
# Selects which example company this container runs, from $OPENCOMPANY_COMPANY.
# The value may be an example directory name (e.g. agentic_venture_capital) or a
# friendly alias (e.g. fund). This is the "which module spins up" switch.
set -eu

COMPANY="${OPENCOMPANY_COMPANY:-agentic_marketing_agency}"

# Friendly aliases → example directory names.
case "$COMPANY" in
  fund | vc | venture-capital)     COMPANY="agentic_venture_capital" ;;
  marketing | agency)              COMPANY="agentic_marketing_agency" ;;
  software | saas | dev)           COMPANY="agentic_software_company" ;;
  studio | venture-studio)         COMPANY="agentic_venture_studio" ;;
  accelerator)                     COMPANY="startup_accelerator" ;;
  law | legal)                     COMPANY="agentic_law_firm" ;;
  accounting | finance)            COMPANY="agentic_accounting_firm" ;;
  support)                         COMPANY="agentic_customer_support" ;;
  signals | opportunity)           COMPANY="signals_opportunity_studio" ;;
esac

DIR="companies/${COMPANY}"
if [ ! -f "${DIR}/company.toml" ] && [ ! -f "${DIR}/agents.toml" ]; then
  echo "opencompany: unknown company '${OPENCOMPANY_COMPANY}' (no manifest at ${DIR})" >&2
  echo "available companies:" >&2
  ls companies | sed 's/^/  - /' >&2
  exit 1
fi

DISCOVER=""
if [ "${OPENCOMPANY_DISCOVERABLE:-false}" = "true" ]; then
  DISCOVER="--discoverable"
fi

echo "opencompany: launching '${COMPANY}' on ${OPENCOMPANY_BIND:-0.0.0.0:8080}"
# shellcheck disable=SC2086
exec opencompany serve \
  --company "${DIR}" \
  --bind "${OPENCOMPANY_BIND:-0.0.0.0:8080}" \
  --home "${OPENCOMPANY_DATA_DIR:-/data}" \
  ${DISCOVER}
