# PR 119 Review Fixes

## Goal

Address every actionable review finding on PR 119 without changing the public
workflow model or weakening existing validation and security boundaries.

## Design

- Workflow TOML config conversion propagates JSON conversion failures from
  `parse_workflow`, so invalid values cannot silently become absent config.
- Workflow state keys use length-prefixed workflow-id and key segments, making
  every pair map to a distinct secret-store key even when either contains `:`.
- GraphQL conversion tests cover node config, error policy, retry camel-case
  fields, and approval state.
- Each workflow execution supplies a unique run identifier to capability
  construction. Tool workspaces remain company-scoped but are isolated beneath
  the workflow and run identifiers.
- Capability construction asynchronously creates the sandbox root required by
  the security policy; tool implementations retain responsibility for their
  own subdirectories.
- Static sub-workflow cycle scanning runs on Tokio's blocking pool and starts
  from the child already parsed by `resolve`, avoiding both executor blocking
  and the duplicate first load.
- Workflow tool construction mirrors agent tool construction and initializes
  only grant-covered shell, code, and web families before applying capability
  filtering.

## Validation

Each behavioral fix gets focused unit coverage. Final verification runs
formatting, Clippy, all-target compilation, the full test suite, and relevant
feature checks.
