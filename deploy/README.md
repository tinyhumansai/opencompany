# Deploying OpenCompany

One image runs the host; a second serves the operator console. **Which company
runs is a single switch — `OPENCOMPANY_COMPANY`** — an example directory name
(`agentic_venture_capital`, `agentic_marketing_agency`, …) or an alias (`fund`,
`marketing`, `software`, `studio`, `law`, `accelerator`, `signals`, …).

The same two images deploy everywhere below; only the wiring differs.

## Local / any Docker host — Compose

```sh
cp .env.example .env
# set OPENCOMPANY_COMPANY to the module you want, then:
docker compose up --build
```

- Console → http://localhost:5173 (proxies the API, so it's same-origin).
- Host API → http://localhost:8080 (e.g. `/healthz`, `/api/v1/companies`).

Switch companies by editing `OPENCOMPANY_COMPANY` in `.env` and re-running
`docker compose up`. Compile optional features into the host with
`OPENCOMPANY_FEATURES="medulla tinyplace sqlite"`.

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
builder, so the `vendor/tinyagents` patch resolves.

### DigitalOcean — plain Droplet

Any Droplet with Docker installed runs the Compose file unchanged:

```sh
git clone <your-fork> && cd opencompany
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
docker build -t <ecr>/opencompany:latest .
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
