#!/bin/sh

# Shared demo-name resolution for the local Docker Compose helpers.
resolve_demo_company() {
    case "$1" in
        fund | vc | venture-capital) echo "agentic_venture_capital" ;;
        marketing | agency) echo "agentic_marketing_agency" ;;
        software | saas | dev) echo "agentic_software_company" ;;
        studio | venture-studio) echo "agentic_venture_studio" ;;
        accelerator) echo "startup_accelerator" ;;
        law | legal) echo "agentic_law_firm" ;;
        accounting | finance) echo "agentic_accounting_firm" ;;
        support) echo "agentic_customer_support" ;;
        signals | opportunity) echo "signals_opportunity_studio" ;;
        *) echo "$1" ;;
    esac
}

demo_project_name() {
    printf 'opencompany-%s\n' "$(printf '%s' "$1" | tr '_' '-')"
}
