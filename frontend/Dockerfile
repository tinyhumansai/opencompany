# syntax=docker/dockerfile:1

# ── build the SPA ──────────────────────────────────────────────────────────
FROM node:22-slim AS build
WORKDIR /app
COPY package.json package-lock.json ./
RUN npm ci
COPY . .
RUN npm run build

# ── serve + proxy the API ──────────────────────────────────────────────────
# nginx serves the static bundle and proxies the API to the host. The upstream
# is configurable so the same image works under Compose (service name),
# single-task ECS (localhost), or any host:port.
FROM nginx:1.27-alpine AS runtime
ENV OC_UPSTREAM=opencompany:8080
COPY nginx/default.conf.template /etc/nginx/templates/default.conf.template
COPY --from=build /app/dist /usr/share/nginx/html
EXPOSE 80
