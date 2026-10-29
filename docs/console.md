# Console and auth

The admin port serves a web console at `/` (for example http://127.0.0.1:9090): mocks, contracts, scenarios, sessions, the live request tail, event channels (a mock's Channels tab: its channels, brokers, triggers, live event feed and manual publish), LLM profiles, API keys, the audit log and server status. It is built from `crates/mb-console/web` and embedded in the binary.

## Auth modes

`auth.mode` is `none` (the default) or `oidc`.

- **`none`:** every admin request acts as the local sysadmin (subject `local`). Fine on loopback; binding another address needs `--insecure-no-auth`.
- **`oidc`:** the console logs in through an OpenID Connect provider (Rauthy by default; any compliant provider works), and API callers send `Authorization: Bearer <token>`, either an OIDC access token (JWT) or a Mockingbird API key. Any bind address is allowed, and the admin port only answers to the host of `auth.external_url`.

```yaml
auth:
  mode: oidc
  external_url: https://mockingbird.example.com   # where browsers reach the admin port
  oidc:
    issuer: https://auth.example.com/auth/v1/      # exactly the discovery `issuer` (Rauthy's ends in /)
    client_id: mockingbird
    client_secret: env:MB_OIDC_CLIENT_SECRET       # a secret reference
```

Every `auth`, `auth.oidc` and `audit` key is in the [configuration reference](configuration.md#auth).

Register `<external_url>/auth/callback` as the client's redirect URI and `<external_url>/` as its post-logout redirect URI. `/healthz`, `/readyz`, `/auth/login`, `/auth/callback` and `/api/openapi.json` are always public, and `/metrics` is too unless `public_metrics: false`. An unauthenticated API call gets a `401` problem; a browser opening a console page is redirected to the login.

## Roles and groups

Each workspace grants one of `viewer < editor < admin < owner`:

| Role | Can |
| --- | --- |
| `viewer` | read everything in the workspace except API keys |
| `editor` | also change mocks, contracts, scenarios, and seed or reset sessions |
| `admin` | also manage LLM profiles and API keys |
| `owner` | also update or delete the workspace |

A **sysadmin** is `owner` in every workspace, and is the only one who can create workspaces or read the global audit log.

Roles come from the `roles_claim` claim (default `groups`) of the ID token or JWT, as strings `<prefix>:<workspace>:<role>` and `<prefix>:sysadmin`. With the default prefix `mb`, a user in the groups `mb:payments:editor` and `mb:default:viewer` is an editor of `payments` and a viewer of `default`, and `mb:sysadmin` makes a sysadmin. A group with the prefix that grants nothing (an unknown role such as `mb:payments:superuser`, a missing segment such as `mb:payments`, or an invalid workspace name) is skipped; a console login logs the skipped groups at info (`login: ignoring prefixed groups that grant no role`), and a bearer token at debug. Groups without the prefix are ignored silently.

## API keys

Workspace admins create keys in the console (**API keys**) or with `POST /api/v1/workspaces/{ws}/api-keys`; only users (or the Local principal in `auth.mode: none`) can, never another API key. A key has a role (at most its creator's), optional expiry, and scopes:

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

A call without a valid, unrevoked `data-plane` key of the mock's workspace (or a sysadmin key) gets `401`. The key is never passed to the engine or recorded. `mode: passthrough_validate` instead requires credentials in the form the contract's security requirements declare, without verifying them against any issuer (only OpenAPI and Swagger contracts declare any). Supported schemes are http bearer and basic, apiKey in a header, query parameter or cookie, oauth2 and openIdConnect (bearer tokens). A requirement that also names another scheme (http digest, mutualTLS) or an undefined one is never satisfied, and the import warns `security_scheme_unsupported`; the operation's other alternatives still apply.

Data-plane auth applies to every protocol: a GraphQL request is admitted (tenant, quota) and then answered `401` before it executes, and the refusal is logged; a GraphQL subscription connection is authenticated once, by the key header or `?api_key=` on the upgrade request (an invalid key is a `401` with no upgrade) or by the `connection_init` payload (the key header name, `api_key` or `apiKey`), and is otherwise refused with close code `4401` (`graphql-transport-ws`) or a `connection_error` (`graphql-ws`); the payload's credential members are never forwarded or logged. A gRPC call reads the key from its request metadata (e.g. `x-api-key: mbk_…`) and fails with `UNAUTHENTICATED`, and a SOAP call (and `GET ?wsdl`) reads it from its HTTP headers and gets a `401` client Fault (`401` problem for the WSDL). The `?api_key=` parameter works for REST, SSE and GraphQL over HTTP and WebSocket.

## Workspaces

Every resource lives in a workspace: `/api/v1/workspaces/{ws}/mocks…`, `…/llm-profiles…`, `…/api-keys…` and `…/audit`. `GET /api/v1/workspaces` lists them, `POST` creates one (sysadmin), and `GET /api/v1/me` shows the caller's roles. Workspace names follow the mock-name rule (`^[a-z0-9][a-z0-9-]{0,62}$`). The `default` workspace always exists and cannot be deleted. Route prefixes stay unique across all workspaces. A mock in the config file picks its workspace with `workspace:` (default `default`).

The P2 paths `/api/v1/mocks…` and `/api/v1/llm-profiles…` still work and act on `default`. They are **deprecated** (and marked so in the OpenAPI document); use the workspace paths.

## Audit log

Every successful change writes one audit event: the actor, the action (`mock.create`, `contract.upload`, `session.reset`, `api_key.create`, `auth.login`, …), the target, and before/after snapshots. Events never contain secrets or tokens. Read them in the console (**Audit**), with `GET /api/v1/workspaces/{ws}/audit` (filters `action`, `actor`, `before`, `limit`), or for all workspaces with `GET /api/v1/audit` (sysadmin). Add `format=jsonl` to export them as JSON Lines. Events older than `audit.retention_days` are pruned daily. Failed console logins (`auth.login_failed`) are audited at most once per source address per 10 s and 100 times per minute overall; the rest only count in `mb_auth_login_failed_suppressed_total`.

With `audit.webhook_url` set, every event is also POSTed there as JSON. Delivery never blocks a request: it is queued, and a failed attempt is retried up to 3 times (1 s, 2 s, then 4 s later) without holding up the events behind it, so a retried event can arrive after later ones (order by its `id`); when the queue is full the event is dropped and `mb_audit_webhook_dropped_total` goes up. The body is signed with `audit.webhook_secret` in `X-Mockingbird-Signature: sha256=<hex HMAC-SHA256(secret, body)>`. Verify it over the raw body, in constant time:

```python
import hashlib, hmac

def verify(secret: bytes, body: bytes, header: str) -> bool:
    expected = "sha256=" + hmac.new(secret, body, hashlib.sha256).hexdigest()
    return hmac.compare_digest(expected, header)
```

## Compose quickstart

`deploy/compose` runs Mockingbird with Postgres and Rauthy, with OIDC login already set up:

```sh
docker compose -f deploy/compose/docker-compose.yml up
```

Open http://localhost:9090 and log in as `demo@localhost.test` / `Demo-Passw0rd-123!`. **The stack's secrets are published dev values: use it only on your own machine.** See [Docker Compose](deploy/compose.md) for the HA and broker profiles.

## Console development

```sh
cd crates/mb-console/web
npm ci
npm run dev        # Vite dev server; proxies the API to a mockingbird on 127.0.0.1:9090
npm test           # Vitest
npm run e2e        # Playwright against the binary with auth.mode: none
npm run build      # into crates/mb-console/dist, which the next cargo build embeds
```

Without a console build, `cargo build` embeds a placeholder page, so Rust work never needs Node. After changing the Admin API, run `npm run check:api` to regenerate the TypeScript types from the OpenAPI document.
