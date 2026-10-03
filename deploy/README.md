# Deploying OpenCompany

One image runs the host; a second serves the operator console. **Which company
runs is a single switch — `OPENCOMPANY_COMPANY`** — an example directory name
(`venture_capital`, `marketing_agency`, …) or an alias (`fund`,
`marketing`, `software`, `studio`, `law`, `accelerator`, `signals`, …).

The self-hosted image defaults to `marketing_agency`. Set
`OPENCOMPANY_COMPANY` **explicitly blank** to boot unconfigured: the host starts
with no company on an empty data root, and the console opens the first-run
**setup wizard**. A routable unconfigured host requires the platform SSO
bootstrap credential; anonymous requests cannot claim it. The hosting platform
injects the blank value and that credential for each new tenant.

The same two images deploy everywhere below; only the wiring differs.

## Local / Docker or Podman — Compose

Everything Docker lives in this directory: `Dockerfile` (built with the
repository root as its context, so run it as `docker build -f deploy/Dockerfile .`
— BuildKit reads `Dockerfile.dockerignore` beside it), `entrypoint.sh`,
`docker-compose.yml`, the `docker-compose.dev.yml` hot-reload overlay and
`.env.example`.

```sh
cd deploy
cp .env.example .env
# set OPENCOMPANY_COMPANY to the module you want, then:
docker compose up --build
```

These commands also work when `docker` is Podman's Docker-compatible CLI and
`podman-compose` is its Compose provider. `scripts/launch-demo.sh` uses this
same portable invocation for the hot-reload stack.

From the repository root, `docker compose -f deploy/docker-compose.yml up --build`
is the same thing — Compose reads `.env` from the compose file's directory
either way.

- Console → http://localhost:5173 (proxies the API, so it's same-origin).
- Host API → http://localhost:8080 (e.g. `/healthz`, `/api/v1/companies`).

There are no default credentials. Open <http://localhost:5173>: a company
nobody has joined yet asks the first visitor to choose the admin login and a
password, and signs them in. Do that before exposing the port to anyone else —
the offer closes the moment the first account exists. Set
`OPENCOMPANY_ADMIN_EMAIL` to restrict the claim to one address.

To create the admin from the shell instead, from the repository root:

```sh
./scripts/init-demo-admin.sh marketing you@example.com
./scripts/launch-demo.sh marketing up
```

Run the initializer once per fresh data volume. A plain `down` preserves the
login; `./scripts/launch-demo.sh marketing down -v` deletes it with the rest of
that demo's data.

Switch companies by editing `OPENCOMPANY_COMPANY` in `.env` and re-running
`docker compose up`. Compile optional features into the host with
`OPENCOMPANY_FEATURES="medulla tinyplace sqlite"`.

To exercise the development Compose flow end to end, including both published
ports, first-admin initialization, and the console's proxy connection to the
host, run:

```sh
./scripts/test-compose-e2e.sh
```

The fast test for the first-admin helper uses a Docker stub and does not start
containers:

```sh
./scripts/test-init-demo-admin.sh
```

The development overlay keeps Cargo and frontend dependency caches in shared
external volumes, while each Compose project retains its own `opencompany-data`
volume. The demo launcher and administrator helper create the cache volumes
automatically when needed, so repeated demo runs do not rebuild dependencies or
remove another project's caches.

The Compose E2E test requires Docker (or a compatible Compose provider), `curl`,
and a working local build environment. It creates a temporary project, asks
Compose for free host ports (`E2E_API_PORT` and `E2E_CONSOLE_PORT` may override
them), initializes `E2E_ADMIN_EMAIL`/`E2E_ADMIN_PASSWORD`, and removes the
project-scoped containers and data volume on exit. Shared dependency caches are
preserved. Run it with `./scripts/test-compose-e2e.sh`; on failure the script
prints the Compose status and logs.

For a selectable memory engine, make sure `OPENCOMPANY_FEATURES` carries
`tinymemory` (it is a default feature; the hosted engines `cortexdb` and
`tinyhumans`, plus the `null` engine), then select one with the
`OPENCOMPANY_MEMORY*` variables (`.env.example` here has the block;
`docs/spec/runtime/memory-engine.md` has the full guide and the
engine-switch runbook).

The console upstream is configurable via `OC_UPSTREAM` (default
`opencompany:8080`), so the console image is portable across every target here.

## DigitalOcean — App Platform

A ready spec is in [`.do/app.yaml`](../.do/app.yaml): the host as a Docker
service (API paths routed to it) and the console as a static site (its
same-origin `/api` calls are routed to the host by App Platform ingress — no
proxy, no CORS).

```sh
doctl apps create --spec .do/app.yaml
# change the company later:
doctl apps update <APP_ID> --spec .do/app.yaml   # after editing OPENCOMPANY_COMPANY
```

Point `github.repo`/`branch` at your fork first. Submodules are cloned by the
builder, so the nested `vendor/openhuman/vendor/tinyagents` patch resolves.

### DigitalOcean — plain Droplet

Any Droplet with Docker installed runs the Compose file unchanged:

```sh
git clone <your-fork> && cd opencompany/deploy
cp .env.example .env && $EDITOR .env
docker compose up -d --build
```

## AWS

### Fargate (ECS)

[`deploy/aws-ecs-task-definition.json`](aws-ecs-task-definition.json) is a
two-container task (host + console in one task; the console reaches the host on
`localhost:8080` via the shared task network). Push both images to ECR, replace
`ACCOUNT_ID`/`REGION`, then register and run:

```sh
# build + push
aws ecr create-repository --repository-name opencompany
aws ecr create-repository --repository-name opencompany-console
docker build -f deploy/Dockerfile -t <ecr>/opencompany:latest .
docker build -t <ecr>/opencompany-console:latest frontend
docker push <ecr>/opencompany:latest && docker push <ecr>/opencompany-console:latest

# deploy
aws ecs register-task-definition --cli-input-json file://deploy/aws-ecs-task-definition.json
aws ecs create-service --cluster <cluster> --service-name opencompany \
  --task-definition opencompany --desired-count 1 --launch-type FARGATE \
  --network-configuration "awsvpcConfiguration={subnets=[...],securityGroups=[...],assignPublicIp=ENABLED}"
```

Change the company by editing `OPENCOMPANY_COMPANY` in the task definition and
re-registering.

**Workspace persistence.** The container's data dir is `/data`
(`OPENCOMPANY_DATA_DIR`, set in the image) — the per-instance workspace root
(`companies/`, `memory/`, `store/`, `files/`, `logs/`, `tmp/`; see
[`storage.md`](../docs/spec/runtime/storage.md)). On Fargate this is ephemeral
unless backed by a volume, so the task definition mounts an **EFS** volume at
`/data`: fill `fileSystemId` (`fs-…`) and `accessPointId` (`fsap-…`) in the
`volumes` block. Give each tenant its **own EFS access point with a storage
cap** — that access-point quota is the *hard* enforcement of
`[workspace].storage_quota_gb` (the workload only alerts when over).

### EC2

Same as any Docker host — run the Compose file on an EC2 instance with Docker.

## Kubernetes / other

The images are plain and stateless except the host's `/data` volume — the
per-instance workspace root (`companies/`, `memory/`, `store/`, `files/`,
`logs/`, `tmp/`; see [`storage.md`](../docs/spec/runtime/storage.md)). Any
orchestrator works: run the host with `OPENCOMPANY_COMPANY` set and a persistent
volume at `/data`, and the console with `OC_UPSTREAM` pointed at the host. On
Kubernetes, back `/data` with a PVC (one tenant per pod, or a shared PVC with a
`subPath` per tenant) and cap it with a `ResourceQuota` / StorageClass quota —
that quota is the hard enforcement of `[workspace].storage_quota_gb`. The host
also honours `TINYHUMANS_API_KEY` (live cognition) and
`OPENCOMPANY_DISCOVERABLE=true` (tiny.place, needs the `tinyplace` feature).
