---
description: Docker, cloud targets, and the hosted platform harness.
---

# Deployment

OpenCompany ships as two images — the host and the operator console — that run
with Docker or Podman.

## Local Docker or Podman (development)

One script spins up a company **and** its console in development mode, attached
to your terminal:

```sh
./scripts/launch-demo.sh marketing up     # console → :5173, host API → :8080
./scripts/launch-demo.sh marketing down   # remove containers + network, keep data
./scripts/launch-demo.sh marketing down -v  # also delete the persistent volume
```

The launcher bind-mounts the local checkout: Vite hot-updates frontend edits,
and `cargo-watch` rebuilds and restarts the backend when Rust source, Cargo
files, or company definitions change. Each company uses a separate Compose
project and persistent data volume. For Podman, install its Docker-compatible
CLI and Compose provider (commonly `podman-docker` and `podman-compose`).

For custom ports, credentials, or feature flags, copy `deploy/.env.example` to `deploy/.env`
before launching.

## Production-like images

Run without source mounts or hot reload:

```sh
OPENCOMPANY_COMPANY=marketing docker compose -f deploy/docker-compose.yml up --build
```

The same two images deploy to:

- **DigitalOcean** — App Platform spec in `.do/app.yaml`.
- **AWS** — Fargate task in `deploy/aws-ecs-task-definition.json`.
- **Any Docker host** — see `deploy/README.md`.

## The hosted platform harness

OpenCompany is also the tenant workload of a hosting platform. A control plane
(`opencompany-manager`) builds this crate into a per-tenant container and
injects its environment:

- `OPENCOMPANY_COMPANY`, `OPENCOMPANY_BIND=0.0.0.0:8080`,
  `OPENCOMPANY_DATA_DIR=/data`, and `OPENCOMPANY_PUBLIC_URL` into every tenant.
- When database-per-tenant storage is on: `OPENCOMPANY_STORAGE=mongodb`,
  `OPENCOMPANY_MONGODB_URI` (credentials scoped to that tenant's database
  only), and `OPENCOMPANY_MONGODB_DB`.
- In shared-single-DB mode: `OPENCOMPANY_TENANT_ID=<tenant-slug>`, and the
  workload namespaces company ids to keep tenants apart. Isolation is
  application-layer only in that mode — database-per-tenant stays the security
  default.

The container must serve `/healthz` on `:8080` quickly — the manager's
wake-on-request proxy blocks on it and gives up after its startup timeout.
Storage backends and the port contracts they implement are documented in the
in-repo spec under `docs/spec/runtime/`.
