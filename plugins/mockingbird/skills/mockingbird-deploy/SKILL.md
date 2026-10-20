---
name: mockingbird-deploy
description: Self-host Mockingbird beyond a laptop - a background server, Docker, the Docker Compose stack (Postgres, OIDC), Kubernetes with the Helm chart (all-in-one or split control/data planes), authentication and API keys, and exposing the MCP server over HTTP for ChatGPT and other remote agents. Use when the user wants a shared, persistent, team or cloud mock endpoint.
---

# Self-host Mockingbird

Pick the smallest shape that fits:

| Need | Shape |
| --- | --- |
| Mocks on this machine while coding | `mockingbird server start -c mockingbird.yaml` (MCP `start_server`) |
| Same, in a container | `docker run` below |
| A shared endpoint for a team or CI, with login | Docker Compose stack, or one VM/container with Postgres and OIDC |
| Production scale or HA | Kubernetes, Helm chart, `mode: split` |

## Local background server

```sh
mockingbird server start -c mockingbird.yaml      # waits for /readyz; prints pid and URLs as JSON
mockingbird server status                         # running, ready, readyz reasons
mockingbird server logs -n 200
mockingbird server stop
```

State (pid, log, ports) lives in `./.mockingbird` (`--state-dir`, `MB_STATE_DIR`). The config file is watched: edits apply without a restart. Use `storage.url: sqlite://./mockingbird.db` (what `init` writes) so mocks and sessions survive restarts.

## Docker

```sh
docker run -d --name mockingbird -p 8080:8080 -p 9090:9090 -p 50051:50051 \
  -e OPENROUTER_API_KEY -v "$PWD:/work" -w /work \
  ghcr.io/rjs-git/mockingbird serve -c mockingbird.yaml --host 0.0.0.0 --insecure-no-auth --watch
```

`--insecure-no-auth` is only for a trusted network: without auth anyone who reaches port 9090 controls the server. Images: `ghcr.io/rjs-git/mockingbird:<version>` (Kafka, AMQP, MQTT) and `:<version>-slim`. Pass provider keys as environment variables (`-e NAME`), never in the config.

## Team server with login

`docker compose -f deploy/compose/docker-compose.yml up` runs Mockingbird with Postgres and the Rauthy OIDC provider (dev secrets; see `docs/deploy/compose.md`). For a real deployment set in `mockingbird.yaml`:

```yaml
storage: { url: postgres://mb@db:5432/mockingbird }   # or MB_STORAGE_URL / --storage-url
auth:
  mode: oidc
  external_url: https://mockingbird.example.com
  oidc: { issuer: https://auth.example.com/, client_id: mockingbird, client_secret: env:MB_OIDC_CLIENT_SECRET }
```

Roles come from the `groups` claim (`mb:<workspace>:<viewer|editor|admin|owner>`, `mb:sysadmin`). Automation uses API keys: create one in the console (API keys) or `admin_api {method: "POST", path: "/workspaces/default/api-keys", body: {name: "ci", role: "editor"}}`; it is shown once. Clients send it with `--token` / `MB_ADMIN_TOKEN` (the MCP server reads the same variable). A mock can require a data-plane key: `data_plane_auth: {mode: api_key}`.

## Kubernetes

```sh
helm install mockingbird oci://ghcr.io/rjs-git/charts/mockingbird --version <version> -f values.yaml
```

The chart is in `deploy/helm/mockingbird` (values in its README). Postgres, Redis and the OIDC issuer are external. `mode: split` runs a control plane (console, Admin API) and an autoscaled data plane; it needs Postgres, and Redis or Postgres for state and bus. Read `docs/deploy/kubernetes.md` before a production install; `docs/operations/runbook.md` explains `/readyz` reasons and alerts.

## MCP for remote agents (ChatGPT, hosted agents)

Local agents run `mockingbird mcp` over stdio. Agents that cannot start a process (ChatGPT connectors and apps, hosted agent platforms) need the MCP server over streamable HTTP, reachable over HTTPS:

```sh
export MB_MCP_TOKEN=$(openssl rand -hex 32)        # bearer token the agent must send
mockingbird mcp --transport http --listen 127.0.0.1:7331 --auth-token-env MB_MCP_TOKEN
# then publish it over HTTPS, e.g.: cloudflared tunnel --url http://127.0.0.1:7331
```

The endpoint is `https://<host>/mcp`. Over HTTP the tools that act on the host machine (config files, local paths, provider tests, the background server) are hidden; `--allow-local-tools` exposes them, which only makes sense when the caller is trusted as much as a local user. Add `--read-only` to expose only the read tools. Binding a non-loopback address requires `--auth-token-env`. In ChatGPT: Settings → Apps & Connectors → Advanced → Developer mode, then create a connector with the URL (and the token, where the client supports a bearer header). The MCP server talks to the Admin API at `--admin-url`, so it can run next to a remote Mockingbird.

## Checklist before sharing an endpoint

- Auth on (`oidc`) or the ports restricted to a trusted network.
- Postgres (or a persistent SQLite path) for storage.
- Provider keys in environment variables or a secret manager, referenced as `env:NAME`.
- `limits` and per-mock `limits.daily_token_budget` set if the LLM is metered.
- `/readyz` wired to the platform's readiness probe; `/metrics` scraped.
