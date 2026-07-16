# App Module

The app module owns process-level configuration and shared Axum state. Keep it
small: it should compose module status and runtime configuration without
absorbing domain behavior from OpenHuman or the `tiny*` crates.

## What `AppState` carries

- **The GraphQL read-plane schema, built once at construction**
  (`build_schema()`) and reused for every `/graphql` request; per-request auth
  (`GqlAuth`) is injected as request data, never rebuilt. `state.schema()`
  hands the prebuilt schema to the handler.
- **The `ConnectionsRuntime` seam** — the injected, dependency-inverted network
  handles for the credential surfaces (DNS resolver, mail sender). Empty by
  default (`ConnectionsRuntime::new()`, the offline build); `serve` populates
  it with real impls under their features, tests with offline mocks. Surfaces
  whose seam is absent degrade to `404 not_wired`.
- The `CompanyRegistry` (`CompanyId` → running `CompanyRuntime`) and the
  platform ownership map, plus module status/spec for `/spec` and `/tiny`.
