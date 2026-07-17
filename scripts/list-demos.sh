#!/bin/sh
# List friendly demo aliases and all company directory names.
set -eu

SCRIPT_DIR=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
REPO_ROOT=$(CDPATH='' cd -- "${SCRIPT_DIR}/.." && pwd)

cat <<'EOF'
Friendly names:
  marketing     agentic_marketing_agency
  software      agentic_software_company
  fund          agentic_venture_capital
  studio        agentic_venture_studio
  accelerator   startup_accelerator
  law           agentic_law_firm
  accounting    agentic_accounting_firm
  support       agentic_customer_support
  signals       signals_opportunity_studio

Company directory names:
EOF

for manifest in "${REPO_ROOT}"/companies/*/company.toml "${REPO_ROOT}"/companies/*/agents.toml; do
    [ -f "$manifest" ] || continue
    basename "$(dirname "$manifest")"
done | sort -u | sed 's/^/  /'
