#!/bin/sh
# Selects which example company this container runs, from $OPENCOMPANY_COMPANY.
# The value may be an example directory name (e.g. venture_capital) or a
# friendly alias (e.g. fund). This is the "which module spins up" switch.
#
# An explicitly blank $OPENCOMPANY_COMPANY means `serve` runs with **no**
# `--company`, so an empty `/data` boots an empty registry and the console opens
# first-run setup. A routable unconfigured launch is authorized by the
# platform's per-tenant SSO bootstrap token. The image default is
# `marketing_agency`; the platform overrides it with an explicit blank. A named
# value selects a baked company as before.
set -eu

COMPANY="${OPENCOMPANY_COMPANY:-}"

BIND="${OPENCOMPANY_BIND:-0.0.0.0:8080}"
HOME_DIR="${OPENCOMPANY_DATA_DIR:-/data}"

if [ -z "${COMPANY}" ]; then
  # Unconfigured launch: no company, empty registry, first-run wizard.
  echo "opencompany: launching unconfigured (setup wizard) on ${BIND}"
  exec opencompany serve \
    --bind "${BIND}" \
    --home "${HOME_DIR}"
fi

# Friendly aliases → example directory names.
case "$COMPANY" in
  fund | vc | venture-capital)     COMPANY="venture_capital" ;;
  marketing | agency)              COMPANY="marketing_agency" ;;
  software | saas | dev)           COMPANY="software_company" ;;
  studio | venture-studio)         COMPANY="venture_studio" ;;
  accelerator)                     COMPANY="startup_accelerator" ;;
  law | legal)                     COMPANY="law_firm" ;;
  accounting | finance)            COMPANY="accounting_firm" ;;
  support)                         COMPANY="customer_support" ;;
  signals | opportunity)           COMPANY="signals_opportunity_studio" ;;
esac

DIR="companies/${COMPANY}"
if [ ! -f "${DIR}/company.toml" ] && [ ! -f "${DIR}/agents.toml" ]; then
  echo "opencompany: unknown company '${OPENCOMPANY_COMPANY}' (no manifest at ${DIR})" >&2
  echo "available companies:" >&2
  ls companies | sed 's/^/  - /' >&2
  exit 1
fi

echo "opencompany: launching '${COMPANY}' on ${BIND}"
exec opencompany serve \
  --company "${DIR}" \
  --bind "${BIND}" \
  --home "${HOME_DIR}"
