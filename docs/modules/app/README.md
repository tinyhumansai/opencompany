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

## The build stamp on `/spec`

`AppSpec` carries `build_commit` beside `version`, because `version` is
`CARGO_PKG_VERSION` and has read `0.1.0` for thousands of commits — enough to
make "a user on 0.1.0 hit this" and "a user on 0.1.0 at `d31e532f` hit this"
the same sentence. It is a short object id, suffixed `-dirty` when tracked
files differed from that commit at build time, or the literal `unknown`.

`build.rs` resolves it once at compile time and emits it as
`OPENCOMPANY_BUILD_COMMIT`; `crate::BUILD_COMMIT` reads it back, and
`opencompany` with no subcommand prints it beside the version. The sources, in
order, each reached only because the one before could not answer:

| Source | Used when | Example |
| --- | --- | --- |
| `OPENCOMPANY_BUILD_COMMIT` | a builder deliberately set it | `release-2026-08-25` |
| `git` | a repository sits beside the crate | `d31e532f7c8a`, `d31e532f7c8a-dirty` |
| `GITHUB_SHA` | there is no usable `.git` — a tarball, a vendored crate, a container context without one | `d31e532f7c8a` |
| — | nothing can answer | `unknown` |

`git` outranks `GITHUB_SHA` on purpose: `GITHUB_SHA` names the ref CI *meant*
to check out, and a workflow checking out something else would stamp a commit
that was never built. **No source is allowed to fail the build** — a host with
no `git` binary at all compiles and reports `unknown`. The dirty suffix comes
only from the `git` branch, since it is the only one describing the same tree
the stamp names.

The reasoning behind putting it on the unauthenticated `/spec` — the
build-fact/deployment-fact line that also keeps `storage` down to a kind — is
written out on the field itself in `src/app/types.rs`. `src/build_stamp.rs`
holds the resolver and its tests; `build.rs` supplies the environment and
`git`, and watches the ref files that keep the stamp from going stale.
