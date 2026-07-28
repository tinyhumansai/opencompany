# PR 119 Review Fixes Implementation Plan

## 1. Preserve workflow config conversion failures

- Add a `parse_workflow` regression test using a TOML non-finite float.
- Replace the fallible iterator mapping with a `Result`-collecting conversion.
- Return a path-labelled parse error instead of silently clearing `config`.
- Run the focused workflow-file tests and commit the file.

## 2. Make workflow state keys unambiguous

- Add a regression test for pairs that previously collided across `:`.
- Length-prefix both namespace segments in `CompanyStateStore::namespaced`.
- Run the focused state tests and commit the file.

## 3. Cover GraphQL node field mapping

- Add a conversion/serialization test for `config`, `on_error`,
  `requires_approval`, and all `RetryGql` fields.
- Assert `maxAttempts` and `backoffMs` camel-case keys.
- Run the focused GraphQL tests and commit the file.

## 4. Isolate workflow run workspaces

- Add an explicit run identifier to `build_capabilities`.
- Generate it once per `run_workflow` invocation.
- Build workspaces beneath the company, workflow, and run identifiers.
- Replace synchronous directory pre-creation with Tokio filesystem I/O.
- Update focused capability/runner tests and commit the touched files.

## 5. Keep cycle scanning off the async executor

- Add or adapt resolver tests covering the existing cycle and budget behavior.
- Change the cycle guard to accept the already-loaded starting workflow.
- Move the remaining bounded filesystem scan into `spawn_blocking` with owned
  source directory, root id, workflow id, and parsed child.
- Run focused resolver tests and commit the file.

## 6. Construct only granted workflow tools

- Add construction-level tests for empty and namespace-specific grants.
- Gate shell, code, and web constructors with the shared `grants_cover` helper.
- Retain capability filtering and invocation-time fail-closed checks.
- Run focused tool tests and commit the file.

## 7. Final verification and publication

- Run formatting, Clippy, all-target build, full tests, and relevant feature
  checks.
- Inspect the final diff and unresolved review state.
- Push the configured branch to update PR 119.
