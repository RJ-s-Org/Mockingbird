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

An unknown scenario name is a 400. Scenarios come from the config file or from `PUT /api/v1/mocks/{mock}/scenarios/{name}`.

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

The request log keeps the newest `observability.request_log_retention` calls per mock. Keys matching `observability.redact_fields` are redacted anywhere in recorded JSON bodies, form bodies and query strings: a key matches when, lowercased and stripped of `_`, `-` and `.`, it contains a field normalized the same way (so `token` also redacts `access_token` and `refresh_token`, and `apiKey` redacts `x-api-key`; it errs towards hiding, e.g. `tokenizer_version`); multipart bodies are not recorded, and a request or response body over 64 KiB is replaced by `{"truncated": true, "bytes": N, "preview": "…"}`. Queued entries are written before a graceful shutdown completes (for up to 5 s).

Changes take effect on the data plane without a restart: activating a contract version or editing a mock swaps its runtime in place.

If a change breaks a mock (for example an LLM profile it uses now names a secret that is not set), the mock keeps serving its previous version, and `/readyz` reports `degraded` with the reason `mock <name>: <error>` until it is fixed. `PUT` and `DELETE` of an LLM profile still succeed in that case, and list the affected mocks in `warnings`. Two mocks may not share a route prefix: creating or moving a mock onto another mock's exact prefix is a 409.

### Admin port safety

There is no authentication yet (it arrives later), so the admin port defends itself against browsers and pasted configs:

- **Host check.** While the admin port is bound to a loopback address (the default), it answers only requests whose `Host` is `localhost`, `127.0.0.1` or `[::1]` (any port); anything else gets `421 Misdirected Request`. This blocks DNS-rebinding attacks from web pages.
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

The image runs as a non-root user and, like the binary, binds loopback unless told otherwise. There is no auth yet, so binding all interfaces needs `--insecure-no-auth`; only do this on a trusted network:

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
| `--insecure-no-auth` | off | Permit a non-loopback bind (there is no auth yet). |
| `--offline` | off | Faker only; no LLM calls. |
| `--llm-timeout` | `3s` | LLM timeout of the default profile before falling back. |

### Environment

| Variable | Default | Meaning |
| --- | --- | --- |
| `MB_HOST`, `MB_PORT`, `MB_ADMIN_PORT`, `MB_STORAGE_URL` | see flags | Same as the flags; flags win. |
| `MB_ADMIN_URL` | `http://127.0.0.1:9090` | Admin API used by the CLI client. |
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
