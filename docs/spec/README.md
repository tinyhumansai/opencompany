# OpenCompany System Specification

OpenCompany is a Rust host for company-aware agent systems built around Axum,
vendored OpenHuman, and vendored TinyAgents.

The goal is to make it easy to start an OpenHuman-backed company runtime,
inspect the inherited runtime modules, and grow company-specific behavior
without duplicating the lower-level OpenHuman and TinyAgents surfaces.

## Detailed Module Docs

- [App module](../modules/app/README.md)
- [Server module](../modules/server/README.md)
- [OpenHuman module](../modules/openhuman/README.md)
- [Tiny module](../modules/tiny/README.md)

Docs should follow the package layout. Do not place standalone specification
files directly in `docs/` or `docs/modules/`; each high-level topic should have
its own directory with a `README.md` entrypoint and any supporting files beside
it.

## Package Layout

```text
src/app/         process configuration and shared state
src/server/      Axum router and HTTP handlers
src/openhuman/   OpenHuman launcher seams
src/tiny/        TinyHumans module feature/status surface
src/bin/         CLI entrypoints
vendor/openhuman/   OpenHuman git submodule
vendor/tinyagents/  TinyAgents git submodule
```

## Design Goals

- Make simple company workflows concise.
- Make complex workflows explicit, inspectable, and testable.
- Reuse OpenHuman and TinyAgents instead of reimplementing their runtime layers.
- Keep the default build small while making deeper inheritance feature-gated.
- Use Axum for the HTTP surface.
- Keep docs, examples, and public APIs aligned.
