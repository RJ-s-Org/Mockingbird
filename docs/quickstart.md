# Quickstart

This page takes you from nothing to a running mock. For what a mock, a session and a scenario are, see [Concepts](concepts.md); to point an app you cannot edit at mocks, see the [Proxy](proxy.md).

## Install

The release binary (macOS, Linux; on Windows use `install.ps1`, see [Coding agents](agents.md#install)):

```sh
curl -fsSL https://raw.githubusercontent.com/rj-s-org/mockingbird/main/install.sh | sh
```

or from a checkout:

```sh
cargo install --path crates/mockingbird
```

## Serve a contract

Serve a contract (OpenAPI 3.x, Swagger 2.0, GraphQL, protobuf or WSDL 1.1; see [Protocols](protocols.md)) with an LLM behind it:

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

Every response carries `X-Mockingbird-Source: llm | repaired | fallback`. A `fallback` response also carries `X-Mockingbird-Fallback-Reason`, which says why the LLM answer was not used (no LLM configured, a timeout, a provider error, output cut at `max_tokens`, or an answer that did not match the schema); GraphQL reports it under `extensions.mockingbird.fallbackReasons`, and the request log records it.

Check a contract without serving it:

```sh
mockingbird validate orders.yaml
```

`validate` ends with a `warning: …` line for each operation whose response schema (with the definitions it reaches) is larger than the 64 KiB the generation prompt shows; the prompt then gets a trimmed schema, while validation still uses the full one. Uploads, the version list, activation and approval return the same warnings (`warnings: [{code: "schema_too_large", operation, message, size}]`).

Mockingbird listens on three ports: the data plane (REST, SSE, GraphQL and SOAP mocks) on `127.0.0.1:8080`, gRPC mocks on `127.0.0.1:50051` (h2c), and the Admin API plus ops endpoints on `127.0.0.1:9090`. If you did not choose an admin port (`--admin-port`, `MB_ADMIN_PORT` or `server.admin_port`) and 9090 is already taken, Mockingbird logs a warning and serves the data plane without the admin port. The gRPC port (`--grpc-port`, `MB_GRPC_PORT` or `server.grpc_port`) works the same way: when 50051 is taken, gRPC mocks are not served. A port you chose explicitly must be free.

## Run it in the background

Write a config with an LLM provider preset and run it as a background server:

```sh
mockingbird init --provider openrouter --contract orders.yaml   # or --offline; `mockingbird llm providers` lists presets
export OPENROUTER_API_KEY=sk-or-...
mockingbird llm test -c mockingbird.yaml --profile openrouter
mockingbird server start -c mockingbird.yaml                     # status, logs, stop
```

## Run your app against mocks

`mockingbird run` starts a mock server with its proxy (unless one is already running), runs your command with the proxy and CA variables set, and exits with the command's status. Mocks whose `hosts:` name your app's API hosts answer its calls; nothing in the app changes:

```sh
mockingbird run -c mockingbird.yaml -- npm start
mockingbird run -c mockingbird.yaml --offline -- npm test   # no LLM key: the built-in faker answers
```

The [Proxy](proxy.md) guide covers what each runtime needs (Node, Python, Go, the JVM, browsers) and how to start the proxy yourself (`mockingbird proxy`, `eval "$(mockingbird proxy env)"`).

## Config file

For anything beyond a quick look, declare the server, storage, LLM profiles and mocks in `mockingbird.yaml`:

```yaml
storage:
  url: sqlite://./mockingbird.db      # or sqlite::memory: (default), postgres://…
llm:
  default_profile: deepseek
  profiles:
    deepseek:
      base_url: https://api.deepseek.com
      model: deepseek-chat
      api_key: env:DEEPSEEK_API_KEY   # a secret reference: env:NAME, file:/path or vault:…
mocks:
  orders:
    contract: ./orders.yaml           # relative to this file
    persona: Terse, realistic retail data.
    scenarios:
      out-of-stock:
        status: 409                   # one orders.yaml declares, or a 5xx
        instructions: Explain that the item is out of stock.
```

The [configuration reference](configuration.md) lists every key; [`docs/examples/mockingbird.yaml`](examples/mockingbird.yaml) is a commented starting point and [`docs/examples/mockingbird.full.yaml`](examples/mockingbird.full.yaml) sets every key.

```sh
mockingbird serve -c mockingbird.yaml          # reconcile once at startup
mockingbird serve -c mockingbird.yaml --watch  # and again whenever the file or a contract changes
```

Mocks, profiles and scenarios declared in the file have `source = file`: they are read-only through the Admin API (409), and the file is the source of truth. Anything created through the API has `source = api` and is left alone by reconciliation. API keys are stored only as references; a raw secret is never written to storage, returned by the API or logged.

Precedence is `mockingbird.yaml` < `MB_*` environment variables < command-line flags.
