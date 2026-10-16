# Mockingbird

A mock-endpoint platform: import an API contract and get a running mock that answers with schema-valid, LLM-generated responses.

## Quickstart

Install from a checkout:

```sh
cargo install --path crates/mockingbird
```

Serve a contract (OpenAPI 3.x or Swagger 2.0) with an LLM behind it:

```sh
MB_LLM_API_KEY=sk-... mockingbird serve orders.yaml
```

Or serve with the built-in faker only, no LLM:

```sh
mockingbird serve --offline orders.yaml
```

Several contracts are each mounted under `/<file stem>`, for example `/a/orders` and `/b/orders`. A single contract is mounted at `/`.

```sh
# Create an order; the response is schema-valid.
curl -i -XPOST localhost:8080/orders \
  -H 'content-type: application/json' \
  -d '{"customerId":"c1","total":10}'

# State is kept per session (default session: `default`).
curl -i localhost:8080/orders/<id-from-above>
curl -i localhost:8080/orders/<id> -H 'X-Mockingbird-Session: alice'

# Force an error status, or add latency.
curl -i localhost:8080/orders/<id> -H 'X-Mockingbird-Status: 404'
curl -i localhost:8080/orders -H 'X-Mockingbird-Latency: 250ms'
```

Every response carries `X-Mockingbird-Source: llm | repaired | fallback`.

Check a contract without serving it:

```sh
mockingbird validate orders.yaml
```

Mockingbird listens on two ports: the data plane (the mocks) on `127.0.0.1:8080`, and the Admin API plus ops endpoints on `127.0.0.1:9090`. If you did not choose an admin port (`--admin-port`, `MB_ADMIN_PORT` or `server.admin_port`) and 9090 is already taken, Mockingbird logs a warning and serves the data plane without the admin port; a port you chose explicitly must be free.

## Config file

For anything beyond a quick look, declare the server, storage, LLM profiles and mocks in `mockingbird.yaml`:

```yaml
server:
  port: 8080
  admin_port: 9090
storage:
  url: sqlite://./mockingbird.db      # or sqlite::memory: (default), postgres://…
  session_ttl: 24h
llm:
  default_profile: deepseek
  profiles:
    deepseek:
      base_url: https://api.deepseek.com
      model: deepseek-chat
      api_key: env:DEEPSEEK_API_KEY   # a secret reference: env:NAME or file:/path
      timeout: 3s
      fallback: local
    local:
      base_url: http://localhost:11434/v1
      model: llama3.1
observability:
  request_log_retention: 1000
mocks:
  orders:
    contract: ./orders.yaml           # relative to this file
    route_prefix: /orders             # default: /<name>
    persona: Terse, realistic retail data.
    scenarios:
      out-of-stock:
        status: 409
        instructions: Explain that the item is out of stock.
      slow:
        latency: 2s
```

```sh
mockingbird serve -c mockingbird.yaml          # reconcile once at startup
mockingbird serve -c mockingbird.yaml --watch  # and again whenever the file or a contract changes
```

Mocks, profiles and scenarios declared in the file have `source = file`: they are read-only through the Admin API (409), and the file is the source of truth. Anything created through the API has `source = api` and is left alone by reconciliation. API keys are stored only as references; a raw secret is never written to storage, returned by the API or logged.

Precedence is `mockingbird.yaml` < `MB_*` environment variables < command-line flags.

## Scenarios

A scenario is a named set of overrides for a mock: extra LLM instructions, a forced status, added latency, fields forced into the body (`set`), optionally limited to some operation ids. Select one per request:

```sh
curl -i localhost:8080/orders/orders/o1 -H 'X-Mockingbird-Scenario: out-of-stock'
```

A scenario's status applies only to operations whose contract declares that status (or a `default` response); other operations are served normally, with the scenario's other overrides. An unknown scenario name is a 400. Scenarios come from the config file or from `PUT /api/v1/mocks/{mock}/scenarios/{name}`.

## Admin API

The Admin API lives under `http://127.0.0.1:9090/api/v1`; its OpenAPI document is served at [`/api/openapi.json`](http://127.0.0.1:9090/api/openapi.json). Every error is an RFC 9457 `application/problem+json` document.

| Resource | Endpoints |
| --- | --- |
| Mocks | `GET/POST /mocks`, `GET/PATCH/DELETE /mocks/{mock}` |
| Contracts | `GET/POST /mocks/{mock}/contracts`, `GET …/contracts/{version}`, `GET …/{version}/source`, `POST …/{version}/activate` |
| Scenarios | `GET /mocks/{mock}/scenarios`, `PUT/DELETE …/scenarios/{name}` |
| LLM profiles | `GET /llm-profiles`, `GET/PUT/DELETE /llm-profiles/{name}` |
| Sessions | `GET /mocks/{mock}/sessions`, `GET/PUT/DELETE …/sessions/{session}` (export, seed, reset) |
| Request log | `GET /mocks/{mock}/requests` |

The request log keeps the newest `observability.request_log_retention` calls per mock. Keys matching `observability.redact_fields` are redacted anywhere in recorded JSON bodies, form bodies and query strings: a key matches when, lowercased and stripped of `_`, `-` and `.`, it contains a field normalized the same way (so `token` also redacts `access_token` and `refresh_token`, and `apiKey` redacts `x-api-key`; it errs towards hiding, e.g. `tokenizer_version`); multipart bodies are not recorded, a request body over 64 KiB is replaced by `{"truncated": true, "bytes": N}`, and a response body over 64 KiB by `{"truncated": true, "bytes": N, "preview": "…"}` (the first 1 KiB of the redacted JSON). Redaction and truncation run on the background writer, never on the request path. Queued entries are written before a graceful shutdown completes (for up to 5 s).

Changes take effect on the data plane without a restart: activating a contract version or editing a mock swaps its runtime in place.

If a change breaks a mock (for example an LLM profile it uses now names a secret that is not set), the mock keeps serving its previous version, and `/readyz` reports `degraded` with the reason `mock <name>: <error>` until it is fixed. `PUT` and `DELETE` of an LLM profile still succeed in that case, and list the affected mocks in `warnings`. Two mocks may not share a route prefix: creating or moving a mock onto another mock's exact prefix is a 409.

### Admin port safety

Besides authentication (see [Console and auth](#console-and-auth)), the admin port defends itself against browsers and pasted configs:

- **Host check.** While the admin port is bound to a loopback address (the default), it answers only requests whose `Host` is `localhost`, `127.0.0.1` or `[::1]` (any port); in `oidc` mode it answers only to the host of `auth.external_url`. Anything else gets `421 Misdirected Request`, except the ops endpoints. This blocks DNS-rebinding attacks from web pages.
- **Content types.** Every mutating request needs `Content-Type: application/json` (activation too, whose body is ignored), and a contract upload needs `application/json`, `application/yaml`, `application/x-yaml`, `text/yaml` or `application/vnd.oai.openapi` (`+json`/`+yaml`); anything else is a `415`. A cross-site form or no-cors request cannot send these, so a web page cannot change mocks behind your back. The CLI client sets them for you; with curl, pass `-H 'content-type: application/json'` (or `application/yaml` for a YAML contract).
- **Secret references.** An LLM profile created or updated through the API may only use secret references matching `server.api_secret_refs` (globs; default `["env:MB_LLM_*", "env:*_API_KEY"]`); `file:` references and other variables are rejected with `422 secret_ref not allowed by server.api_secret_refs`. Profiles in the config file are not restricted. Widen the list in `mockingbird.yaml` if you need to:

  ```yaml
  server:
    api_secret_refs: ["env:MB_LLM_*", "env:*_API_KEY", "file:/run/secrets/*"]
  ```

## CLI client

The same binary drives the Admin API. It prints JSON on stdout and problem details on stderr, and exits 1 on any non-2xx response. The target is `--admin-url`, then `MB_ADMIN_URL`, then `http://127.0.0.1:9090`.

```sh
mockingbird mocks list
mockingbird mocks get orders
mockingbird mocks create billing --prefix /bill --persona "Formal" --profile deepseek
mockingbird mocks delete billing

# Upload a contract as a new version (creating the mock if missing).
mockingbird import orders.yaml --as orders --activate

mockingbird sessions list orders
mockingbird sessions reset orders alice

# Save a session's state as fixtures, and load it back (into any session).
mockingbird export-fixtures --mock orders --session alice > fixtures.json
mockingbird seed-fixtures --mock orders --session ci fixtures.json
```

## Console and auth

The admin port serves a web console at `/` (for example http://127.0.0.1:9090): mocks, contracts, scenarios, sessions, the live request tail, LLM profiles, API keys, the audit log and server status. It is built from `crates/mb-console/web` and embedded in the binary.

### Auth modes

`auth.mode` is `none` (the default) or `oidc`.

- **`none`:** every admin request acts as the local sysadmin (subject `local`). Fine on loopback; binding another address needs `--insecure-no-auth`.
- **`oidc`:** the console logs in through an OpenID Connect provider (Rauthy by default; any compliant provider works), and API callers send `Authorization: Bearer <token>`, either an OIDC access token (JWT) or a Mockingbird API key. Any bind address is allowed, and the admin port only answers to the host of `auth.external_url`.

```yaml
auth:
  mode: oidc
  external_url: https://mockingbird.example.com   # where browsers reach the admin port
  session_ttl: 12h                                 # absolute console session lifetime
  sysadmin_subjects: ["3f2c…"]                     # OIDC `sub` values that are always sysadmin
  public_metrics: true                             # serve /metrics without credentials
  oidc:
    issuer: https://auth.example.com/auth/v1
    client_id: mockingbird
    client_secret: env:MB_OIDC_CLIENT_SECRET       # env:NAME or file:/path
    scopes: [openid, profile, email, groups]       # default
    roles_claim: groups                            # default
    group_prefix: mb                               # default
    audience: mockingbird                          # `aud` of bearer JWTs; default: client_id
audit:
  retention_days: 365
  webhook_url: https://hooks.example.com/mockingbird
  webhook_secret: env:MB_AUDIT_WEBHOOK_SECRET
```

Register `<external_url>/auth/callback` as the client's redirect URI and `<external_url>/` as its post-logout redirect URI. `/healthz`, `/readyz`, `/auth/login`, `/auth/callback` and `/api/openapi.json` are always public, and `/metrics` is too unless `public_metrics: false`. An unauthenticated API call gets a `401` problem; a browser opening a console page is redirected to the login.

### Roles and groups

Each workspace grants one of `viewer < editor < admin < owner`:

| Role | Can |
| --- | --- |
| `viewer` | read everything in the workspace except API keys |
| `editor` | also change mocks, contracts, scenarios, and seed or reset sessions |
| `admin` | also manage LLM profiles and API keys |
| `owner` | also update or delete the workspace |

A **sysadmin** is `owner` in every workspace, and is the only one who can create workspaces or read the global audit log.

Roles come from the `roles_claim` claim (default `groups`) of the ID token or JWT, as strings `<prefix>:<workspace>:<role>` and `<prefix>:sysadmin`. With the default prefix `mb`, a user in the groups `mb:payments:editor` and `mb:default:viewer` is an editor of `payments` and a viewer of `default`, and `mb:sysadmin` makes a sysadmin.

### API keys

Workspace admins create keys in the console (**API keys**) or with `POST /api/v1/workspaces/{ws}/api-keys`. A key has a role (at most its creator's), optional expiry, and scopes:

- **`admin`** (the default): the key authenticates Admin API calls, with its role, like a user.
- **`data-plane`**: the key unlocks mocks whose `data_plane_auth.mode` is `api_key` (see below).

The token, `mbk_<key_id>_<secret>`, is shown once; only a hash is stored. The CLI client sends one with `--token` (or `MB_ADMIN_TOKEN`):

```sh
mockingbird --admin-url https://mockingbird.example.com --token "$MB_ADMIN_TOKEN" mocks list
```

A mock can require a data-plane key on every call:

```yaml
mocks:
  orders:
    contract: ./orders.yaml
    data_plane_auth: { mode: api_key, header: x-api-key }   # or ?api_key=…
```

A call without a valid, unrevoked `data-plane` key of the mock's workspace (or a sysadmin key) gets `401`. The key is never passed to the engine or recorded. `mode: passthrough_validate` instead requires credentials in the form the contract's security requirements declare, without verifying them against any issuer.

### Workspaces

Every resource lives in a workspace: `/api/v1/workspaces/{ws}/mocks…`, `…/llm-profiles…`, `…/api-keys…` and `…/audit`. `GET /api/v1/workspaces` lists them, `POST` creates one (sysadmin), and `GET /api/v1/me` shows the caller's roles. Workspace names follow the mock-name rule (`^[a-z0-9][a-z0-9-]{0,62}$`). The `default` workspace always exists and cannot be deleted. Route prefixes stay unique across all workspaces. A mock in the config file picks its workspace with `workspace:` (default `default`).

The P2 paths `/api/v1/mocks…` and `/api/v1/llm-profiles…` still work and act on `default`. They are **deprecated** (and marked so in the OpenAPI document); use the workspace paths.

### Audit log

Every successful change writes one audit event: the actor, the action (`mock.create`, `contract.upload`, `session.reset`, `api_key.create`, `auth.login`, …), the target, and before/after snapshots. Events never contain secrets or tokens. Read them in the console (**Audit**), with `GET /api/v1/workspaces/{ws}/audit` (filters `action`, `actor`, `before`, `limit`), or for all workspaces with `GET /api/v1/audit` (sysadmin). Add `format=jsonl` to export them as JSON Lines. Events older than `audit.retention_days` are pruned daily.

With `audit.webhook_url` set, every event is also POSTed there as JSON. Delivery never blocks a request: it is queued, and a failed attempt is retried up to 3 times; when the queue is full the event is dropped and `mb_audit_webhook_dropped_total` goes up. The body is signed with `audit.webhook_secret` in `X-Mockingbird-Signature: sha256=<hex HMAC-SHA256(secret, body)>`. Verify it over the raw body, in constant time:

```python
import hashlib, hmac

def verify(secret: bytes, body: bytes, header: str) -> bool:
    expected = "sha256=" + hmac.new(secret, body, hashlib.sha256).hexdigest()
    return hmac.compare_digest(expected, header)
```

### Compose quickstart

`deploy/compose` runs Mockingbird with Postgres and Rauthy, with OIDC login already set up:

```sh
docker compose -f deploy/compose/docker-compose.yml up
```

Open http://localhost:9090 and log in as `demo@localhost.test` / `Demo-Passw0rd-123!` (a sysadmin and an admin of `default`). The data plane is on http://localhost:8080. **The stack's secrets are published dev values: use it only on your own machine.** See [deploy/compose/README.md](deploy/compose/README.md) for details and manual setup steps.

### Console development

```sh
cd crates/mb-console/web
npm ci
npm run dev        # Vite dev server; proxies the API to a mockingbird on 127.0.0.1:9090
npm test           # Vitest
npm run e2e        # Playwright against the binary with auth.mode: none
npm run build      # into crates/mb-console/dist, which the next cargo build embeds
```

Without a console build, `cargo build` embeds a placeholder page, so Rust work never needs Node. After changing the Admin API, run `npm run check:api` to regenerate the TypeScript types from the OpenAPI document.

## Ops endpoints

On the admin port:

| Path | Meaning |
| --- | --- |
| `/healthz` | Liveness: the process is up. |
| `/readyz` | Readiness: 503 `unavailable` when the control store is unreachable; otherwise 200 with `ready`, or `degraded` plus reasons when the state store is failing and mocks serve statelessly, or a mock failed to rebuild and serves its previous version. Each store ping is bounded to 2 s. |
| `/metrics` | Prometheus metrics: requests, latency, LLM tokens and outcomes, request-log drops. |

The data plane keeps `/__mockingbird/healthz`. Set `OTEL_EXPORTER_OTLP_ENDPOINT` to export traces over OTLP (HTTP/protobuf).

## Postgres

SQLite is the default (in memory, or `sqlite://<path>`, created if missing). For shared or long-lived deployments point storage at Postgres; migrations run at startup under an advisory lock:

```sh
mockingbird serve -c mockingbird.yaml --storage-url postgres://mb:secret@db:5432/mockingbird
# or MB_STORAGE_URL=postgres://…
```

Session state uses the same database unless `storage.state_url` says otherwise (`memory` keeps it in process).

To run the Postgres-gated tests against a throwaway local server: `scripts/test-postgres.sh`. Otherwise they read `MB_TEST_POSTGRES_URL` and skip when it is unset.

## Docker

```sh
docker build -t mockingbird .
docker run --rm mockingbird --version
```

The image builds the console too. It runs as a non-root user and, like the binary, binds loopback unless told otherwise. With `auth.mode: none`, binding all interfaces needs `--insecure-no-auth`; only do this on a trusted network (or use `oidc`, as in the [Compose stack](deploy/compose/README.md)):

```sh
docker run --rm -p 8080:8080 -p 9090:9090 \
  -v "$PWD/orders.yaml:/work/orders.yaml:ro" \
  mockingbird serve --host 0.0.0.0 --insecure-no-auth --offline /work/orders.yaml
```

With a config file, mount it and its contracts and pass `-c`; use Postgres (`MB_STORAGE_URL`) to keep state across container restarts.

## Reference

### Flags

`mockingbird serve`:

| Flag | Default | Meaning |
| --- | --- | --- |
| `-c, --config` | none | Config file; without it the contracts given are served zero-config. |
| `--watch` | off | Reconcile again when the config file or a contract changes (needs `-c`). |
| `--host` | `127.0.0.1` | Bind address of both ports. A non-loopback host requires `--insecure-no-auth`. |
| `--port` | `8080` | Data-plane port. |
| `--admin-port` | `9090` | Admin API and ops port. |
| `--storage-url` | `sqlite::memory:` | Control-plane storage. |
| `--insecure-no-auth` | off | Permit a non-loopback bind with `auth.mode: none`. |
| `--offline` | off | Faker only; no LLM calls. |
| `--llm-timeout` | `3s` | LLM timeout of the default profile before falling back. |

### Environment

| Variable | Default | Meaning |
| --- | --- | --- |
| `MB_HOST`, `MB_PORT`, `MB_ADMIN_PORT`, `MB_STORAGE_URL` | see flags | Same as the flags; flags win. |
| `MB_ADMIN_URL` | `http://127.0.0.1:9090` | Admin API used by the CLI client. |
| `MB_ADMIN_TOKEN` | none | API key the CLI client sends (`--token`). |
| `MB_LLM_API_KEY` | none | API key of the default profile. Required unless `--offline`; local endpoints such as Ollama accept any dummy value. |
| `MB_LLM_BASE_URL` | `https://api.deepseek.com` | OpenAI-compatible base URL. |
| `MB_LLM_MODEL` | `deepseek-chat` | Model name. |
| `MB_LLM_JSON_SCHEMA` | `false` | `true` to request JSON Schema mode instead of JSON mode. |
| `RUST_LOG` | `info` | Log filter. |
| `OTEL_EXPORTER_OTLP_ENDPOINT` | none | Enables OTLP trace export. |

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.
