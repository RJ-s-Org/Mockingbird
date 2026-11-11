# Quickstart

This page takes you from nothing to a running mock. For what a mock, a session and a scenario are, see [Concepts](concepts.md); to point an app you cannot edit at mocks, see the [Proxy](proxy.md).

## Install

In a Node.js project, run it through npm with nothing to install (see [Install](install.md) for every channel):

```sh
npx @mockingbird-cli/mockingbird run --offline -- npm test
```

Or the release binary (macOS, Linux; on Windows use `install.ps1`, see [Coding agents](agents.md#install)):

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
MB_LLM_API_KEY=sk-or-... mockingbird serve orders.yaml   # an OpenRouter key; MB_LLM_BASE_URL and MB_LLM_MODEL pick another endpoint
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

Every response carries `X-Mockingbird-Source: llm | patched | repaired | fallback`. A `fallback` response also carries `X-Mockingbird-Fallback-Reason`, which says why the LLM answer was not used (no LLM configured, a timeout, a provider error, output cut at `max_tokens`, or an answer that did not match the schema); GraphQL reports it under `extensions.mockingbird.fallbackReasons`, and the request log records it.

Check a contract without serving it:

```sh
mockingbird validate orders.yaml
```

`validate` ends with a `warning: …` line for each operation whose response schema (with the definitions it reaches) is larger than the 64 KiB the generation prompt shows; the prompt then gets a trimmed schema, while validation still uses the full one. The schema of an answer the model writes is fitted to 24 KiB: a member that is either an id string or a related object (Stripe's expandable fields) shows only the id unless the request's `expand` names it, the definitions behind members that may be null become stubs first, and descriptions are cut. Uploads, the version list, activation and approval return the same warnings (`warnings: [{code: "schema_too_large", operation, message, size}]`). Other warnings say what the import relaxed or skipped, such as an expanded `4XX` response range or a stubbed dangling `$ref`; see [Import warnings](protocols.md#import-warnings).

Mockingbird listens on three ports: the data plane (REST, SSE, GraphQL and SOAP mocks) on `127.0.0.1:8080`, gRPC mocks on `127.0.0.1:50051` (h2c), and the Admin API plus ops endpoints on `127.0.0.1:9090`. If you did not choose an admin port (`--admin-port`, `MB_ADMIN_PORT` or `server.admin_port`) and 9090 is already taken, `mockingbird serve` logs a warning and serves the data plane without the admin port. The gRPC port (`--grpc-port`, `MB_GRPC_PORT` or `server.grpc_port`) works the same way: when 50051 is taken, gRPC mocks are not served. A port you chose explicitly must be free.

Another project's Mockingbird server often holds the default ports. `mockingbird init` and `mockingbird server start -c` handle that for you:

- `init` checks each default port it would leave unset (`server.port`, `server.admin_port`, `server.grpc_port`). When one is taken, it writes the first free port above it into the config and lists it under `ports_chosen`. A port you pass (`--port`, `--admin-port`, `--grpc-port`) is written as given; when it is taken, the output says so under `warnings`.
- `server start -c mockingbird.yaml` does the same for each port that no flag, `MB_*` variable or config key sets, and for `proxy.port` when the proxy runs (`--proxy` or `proxy.enabled`). It writes the ports it picks into the config and lists them under `ports_chosen`. Because they are in the config, the client subcommands (`server status`, `mocks`, `import`, ...), `proxy env`, `run` and the MCP server all find this project's server there; nothing assumes 9090. A port that is set and taken fails the start before anything is spawned. The error names the listener and address and, when the port belongs to another Mockingbird server, that server's config and folder (from its `/readyz`). This project's commands never use that server.
- `mockingbird run -c mockingbird.yaml` reuses a server already answering at the config's admin port only when it was started with that config. A server started with another config is left alone, and `run` starts this config's own server on free ports.

`proxy env`, `proxy browser` and `proxy` read `./mockingbird.yaml` when you give no `-c`, so they find the proxy port written there.

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
  default_profile: openrouter
  profiles:
    openrouter:
      base_url: https://openrouter.ai/api/v1
      model: deepseek/deepseek-v4.1-flash-1017
      api_key: env:OPENROUTER_API_KEY # a secret reference: env:NAME, file:/path or vault:…
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
