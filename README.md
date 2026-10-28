# Mockingbird

A mock-endpoint platform: import an API contract and get a running mock that answers with schema-valid, LLM-generated responses.

It mocks REST, GraphQL, gRPC, SOAP, SSE, WebSocket and event channels (Kafka, AMQP, MQTT) from their contracts, keeps per-session state, and runs as one binary or as a highly available control and data plane on Kubernetes.

## Documentation

| Guide | What it covers |
| --- | --- |
| [Coding agents](docs/agents.md) | The MCP server, skills and plugins for Claude Code, Codex, Cursor, Gemini CLI, Kiro, VS Code, Windsurf and ChatGPT; LLM provider onboarding |
| [Architecture](docs/architecture.md) | Crates, roles, the request flow, HA (bus, leader election) and state backends |
| [Configuration reference](docs/configuration.md) | Every `mockingbird.yaml` key, with its default, range, environment variable and flag |
| [Webhooks](docs/webhooks.md) | Signed events (Stripe-style or HMAC) POSTed after writes, retries, `webhooks sign` and the delivery log |
| [Proxy](docs/proxy.md) | `mockingbird proxy`: reaching mocks by real host names (`hosts:`) over HTTPS with a local CA, per runtime |
| [CI](docs/ci.md) | The GitHub Action, GitLab template and other CI recipes; record once, replay without an LLM key; `report` and `verify` |
| [Running locally](docs/deploy/local.md) | The binary, `serve`, offline mode and TLS |
| [Docker Compose](docs/deploy/compose.md) | The Compose stack and its `ha`, `kafka`, `amqp` and `mqtt` profiles |
| [Kubernetes](docs/deploy/kubernetes.md) | The Helm chart, external stores, OIDC, ingress, secrets and verifying releases |
| [Infrastructure as code](docs/iac.md) | The Terraform/OpenTofu provider and Pulumi: mocks, contracts, scenarios and keys from code; a mocked staging environment in 20 lines |
| [Operations runbook](docs/operations/runbook.md) | Signals, what they mean and what to do |
| [Upgrading](docs/upgrading.md) | Upgrade notes, migrations and the `mb-ext` 1.0 changes |
| [Performance](docs/performance.md) | The offline bench: method, numbers and how to rerun it |
| [Improvements](docs/improvements.md) | Known gaps and planned enhancements, from field trials and use |
| [Extensions](crates/mb-ext/README.md) | The `mb-ext` extension API and its stability policy |
| [Examples](examples/gallery/README.md) | Ready-to-serve configurations for every protocol and for chaos |

## Quickstart

Install the release binary (macOS, Linux; on Windows use `install.ps1`, see [Coding agents](docs/agents.md#install)):

```sh
curl -fsSL https://raw.githubusercontent.com/rj-s-org/mockingbird/main/install.sh | sh
```

or from a checkout:

```sh
cargo install --path crates/mockingbird
```

Write a config with an LLM provider preset, then run it in the background:

```sh
mockingbird init --provider openrouter --contract orders.yaml   # or --offline; `mockingbird llm providers` lists presets
export OPENROUTER_API_KEY=sk-or-...
mockingbird llm test -c mockingbird.yaml --profile openrouter
mockingbird server start -c mockingbird.yaml                     # status, logs, stop
```

Serve a contract (OpenAPI 3.x, Swagger 2.0, GraphQL, protobuf or WSDL 1.1; see [Protocols](#protocols)) with an LLM behind it:

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

## Coding agents

Mockingbird is built to be driven by coding agents. `mockingbird mcp` is an MCP server whose tools cover the whole product: install checks (`doctor`), LLM onboarding (OpenRouter, Google AI Studio, Cloudflare AI Gateway and Workers AI, the major vendors, local runtimes, or any OpenAI-compatible endpoint with your own key), the background server, and every Admin API operation. Skills tell the agent when to use them.

```sh
/plugin marketplace add rj-s-org/mockingbird          # Claude Code, then /plugin install mockingbird@mockingbird
mockingbird agent install codex                      # or cursor, gemini, kiro, vscode, windsurf, claude-desktop
mockingbird mcp --transport http --auth-token-env MB_MCP_TOKEN   # remote agents such as ChatGPT
```

See [Coding agents](docs/agents.md) for each agent, the tool list and the security model of the HTTP transport.

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

The [configuration reference](docs/configuration.md) lists every key; [`docs/examples/mockingbird.yaml`](docs/examples/mockingbird.yaml) is a commented starting point and [`docs/examples/mockingbird.full.yaml`](docs/examples/mockingbird.full.yaml) sets every key.

```sh
mockingbird serve -c mockingbird.yaml          # reconcile once at startup
mockingbird serve -c mockingbird.yaml --watch  # and again whenever the file or a contract changes
```

Mocks, profiles and scenarios declared in the file have `source = file`: they are read-only through the Admin API (409), and the file is the source of truth. Anything created through the API has `source = api` and is left alone by reconciliation. API keys are stored only as references; a raw secret is never written to storage, returned by the API or logged.

Precedence is `mockingbird.yaml` < `MB_*` environment variables < command-line flags.

## Protocols

The importer detects a contract's format from its bytes (the file name does not matter), and the mock is served by the adapter for that protocol family:

| Format | Detected by | Served |
| --- | --- | --- |
| OpenAPI 3.x, Swagger 2.0 (JSON or YAML) | an `openapi: 3.*` or `swagger: "2.0"` key | REST at the route prefix. Operations whose success response is `text/event-stream` stream server-sent events (SSE). |
| GraphQL SDL | SDL definitions (`type Query`, `schema {`, …) | GraphQL at `<prefix>/graphql` (`protocols.graphql.path`): queries over `GET` or `POST`, mutations over `POST`, subscriptions over a WebSocket upgrade of the same path (`graphql-transport-ws` or the legacy `graphql-ws`). |
| GraphQL introspection JSON | a `__schema` or `data.__schema` key | As GraphQL SDL. |
| protobuf `.proto` | a `syntax = "proto…"` statement, or `service` and `message` definitions | gRPC on `:50051` (all four call kinds), with server reflection and the standard health service. The route prefix is ignored. |
| protobuf `FileDescriptorSet` | a binary descriptor set (`protoc --descriptor_set_out`) | As `.proto`. |
| AsyncAPI 2.0–2.6, 3.0 (JSON or YAML) | an `asyncapi: 2.*` or `asyncapi: 3.0.*` key | Event channels: generated messages published to Kafka, AMQP or MQTT and consumed from them, or WebSocket channels at `<prefix><address>`. See [Events](#events). |
| WSDL 1.1 (with inline or bundled XSD) | an XML `definitions` root in the WSDL 1.1 namespace | SOAP 1.1 and 1.2 at `<prefix><address path>`, the path of each service's `soap:address`; `GET <prefix><address path>?wsdl` returns the WSDL with its addresses pointing at the mock. |
| zip or tar.gz archive | the zip or gzip magic bytes | The contract inside: all `.proto` files compiled together, else the single `.wsdl` with its XSDs, else the `.graphql`/`.graphqls`/`.gql` files joined as one schema, else the single file. See [Archive uploads](#archive-uploads). |

Operation ids, as scenarios' `operations` lists and the request log use them, are `Query.<field>` / `Mutation.<field>` / `Subscription.<field>` for GraphQL, `<Service>.<Method>` for gRPC (`<package>.<Service>.<Method>` when the short form collides), and `<operation>` for SOAP (`<Service>.<operation>` on collision). Each gRPC service is served by one mock across every workspace (the gRPC listener is shared): uploading with activation, or activating, a contract whose services another mock's active contract serves is a 409, and a config file declaring such a clash is rejected. If a clash reaches the data plane anyway, the mock already serving the service keeps it and the newcomer is reported in `/readyz` as failed. gRPC reflection lists only the services the caller's data-plane key reaches (plus mocks without data-plane auth) and needs a valid key when any gRPC mock is in `api_key` mode.

### Examples

The examples serve one contract each, so it is mounted at `/`; with a config file, put the mock's route prefix in front of each path.

GraphQL (`mockingbird serve --offline starwars.graphql`):

```sh
curl -s localhost:8080/graphql \
  -H 'content-type: application/json' \
  -d '{"query":"{ hero(episode: JEDI) { name } }"}'
```

The sources of the root fields are reported in the response, under `extensions.mockingbird.sources`.

gRPC (`mockingbird serve --offline helloworld.proto`):

```sh
grpcurl -plaintext 127.0.0.1:50051 list
grpcurl -plaintext -d '{"name":"world"}' \
  -H 'x-mockingbird-session: alice' \
  127.0.0.1:50051 helloworld.Greeter/SayHello
```

SOAP 1.1 (`mockingbird serve --offline calculator.wsdl`, whose address is `http://www.example.com/calculator.asmx`):

```sh
curl -s localhost:8080/calculator.asmx \
  -H 'content-type: text/xml; charset=utf-8' \
  -H 'SOAPAction: "http://tempuri.org/Add"' \
  -d '<soap:Envelope xmlns:soap="http://schemas.xmlsoap.org/soap/envelope/">
        <soap:Body><Add xmlns="http://tempuri.org/"><intA>1</intA><intB>2</intB></Add></soap:Body>
      </soap:Envelope>'
curl -s 'localhost:8080/calculator.asmx?wsdl'
```

SOAP 1.2 sends `content-type: application/soap+xml; action="…"` instead of `SOAPAction`. Every failure is a SOAP Fault: `Client`/`Sender` for a 4xx status, `Server`/`Receiver` otherwise.

SSE (an OpenAPI operation answering `text/event-stream`):

```sh
curl -N localhost:8080/ticks -H 'X-Mockingbird-Stream-Items: 3'
```

### Streaming

SSE responses, GraphQL subscriptions and gRPC server- and bidi-streaming calls are generated as a stream of items, each validated as it completes (repaired once, or replaced by faker output). The number of items comes from, in order:

1. the `X-Mockingbird-Stream-Items` header (gRPC metadata `x-mockingbird-stream-items`; an integer from 1 to 10000);
2. the selected scenario's `stream_items`;
3. `protocols.stream.default_items` (5).

The count is capped at `protocols.stream.max_items` (50). A scenario's `stream_item_interval`, or else `protocols.stream.item_interval` (0), is the pause between items.

SSE streams and GraphQL subscriptions stay open after the generated items: they go on to deliver resources of the operation's type that are created or changed in the same session, until the client disconnects. A gRPC server stream completes after its last item. Generated stream items are never written to session state.

### Override mapping per protocol

The override headers work on every protocol: gRPC reads them as lowercased metadata (`x-mockingbird-session`, `x-mockingbird-status`, …), and GraphQL reads them from the HTTP request (or from the `connection_init` payload of a subscription). A forced status is reported natively.

REST answers a forced status with the operation's declared response for it (or its `default` response). A 5xx the operation does not declare is answered as a plain `application/problem+json` document with that status, since any API can fail that way; any other undeclared status is a `400 Undeclared status override`, and a value that is not a status code from 100 to 599 is a `400 Invalid override`.

gRPC answers with the status code standing for the HTTP status:

| HTTP status | gRPC status |
| --- | --- |
| 400, and any 4xx not listed | `INVALID_ARGUMENT` |
| 401 | `UNAUTHENTICATED` |
| 403 | `PERMISSION_DENIED` |
| 404 | `NOT_FOUND` |
| 409 | `ALREADY_EXISTS` |
| 412 | `FAILED_PRECONDITION` |
| 416 | `OUT_OF_RANGE` |
| 429 | `RESOURCE_EXHAUSTED` |
| 499 | `CANCELLED` |
| 501 | `UNIMPLEMENTED` |
| 503 | `UNAVAILABLE` |
| 504 | `DEADLINE_EXCEEDED` |
| any other | `INTERNAL` |

GraphQL answers `200` with a field error whose `extensions.code` stands for the status (and `extensions.status` holds the status itself):

| HTTP status | `extensions.code` |
| --- | --- |
| 400 | `BAD_USER_INPUT` |
| 401 | `UNAUTHENTICATED` |
| 403 | `FORBIDDEN` |
| 404 | `NOT_FOUND` |
| 409 | `CONFLICT` |
| 429 | `RATE_LIMITED` |
| any other 4xx | `BAD_REQUEST` |
| 5xx | `INTERNAL_SERVER_ERROR` |

Request limits: GraphQL bodies 1 MiB, query depth 15 and complexity 1000 (`protocols.graphql`); gRPC messages 4 MiB (`protocols.grpc.max_message_bytes`); SOAP bodies 4 MiB.

### Known limits

- WSDL 2.0 is not supported; only WSDL 1.1 with SOAP 1.1/1.2 bindings is.
- External XSDs are never fetched: a `schemaLocation` must name a file uploaded with the WSDL in the same archive.
- Complex XSD `pattern` facets may yield faker output that does not validate against them; Mockingbird logs a warning when that happens.
- Token usage of streamed generations is not metered.

## Events

An AsyncAPI contract makes a mock event-driven: each channel operation becomes a *channel* that Mockingbird either publishes generated messages on or consumes, validates and records messages from. Channels run next to the mock's other protocols and share its session state, LLM profile and persona.

### AsyncAPI support

AsyncAPI 2.0–2.6 and 3.0, in JSON or YAML (detected by the `asyncapi:` key), with `$ref`s inside the document. Channel and operation bindings for `kafka` (topic, key schema), `amqp` (exchange or queue, routing key) and `mqtt` (topic, QoS, retain) are read, and `ws` channels are served over WebSocket; without a binding, the protocol of the channel's server is used. Message payloads may be JSON Schema or Avro (`schemaFormat: application/vnd.apache.avro…`), which is converted to JSON Schema. Message headers, Kafka keys and `{parameter}`s in addresses are generated from their schemas.

AsyncAPI describes operations from the application's point of view, which is the point of view of the service Mockingbird stands in for:

| AsyncAPI 2.x | AsyncAPI 3.0 | Mockingbird |
| --- | --- | --- |
| `subscribe` (clients receive) | `action: send` | **publishes** generated messages |
| `publish` (clients send) | `action: receive` | **subscribes**: consumes, validates and records messages, optionally replying or changing state |

The channel id is the operation id (`operationId` in 2.x, the operation key in 3.0).

### Brokers

Brokers are declared once in the config file and read at startup. With `--watch`, a reload whose `brokers:` section differs from the running one (a broker added, removed or changed) is rejected as a whole with `brokers changed: restart required`: nothing else in that edit is applied, the previous state stays, and the watch keeps running; restart Mockingbird to change brokers.

```yaml
brokers:
  kafka:
    kind: kafka
    bootstrap_servers: kafka-1:9092,kafka-2:9092
    tls: true
    sasl: {mechanism: SCRAM-SHA-512, username: mockingbird, password: env:KAFKA_PASSWORD}
  rabbit:
    kind: amqp
    url: amqp://mockingbird@127.0.0.1:5672/%2f
  mosquitto:
    kind: mqtt
    url: mqtt://127.0.0.1:1883
  local:
    kind: memory        # in-process; nothing leaves Mockingbird
```

`kind` is `kafka`, `amqp`, `mqtt` or `memory`. Passwords are secret references in `sasl.password`, never part of a URL. MQTT brokers are reached over plain TCP only: this build's MQTT client (rumqttc) is compiled without its TLS stack, whose `rustls-webpki` 0.102 has open security advisories, so an `mqtts://` or `ssl://` URL, `tls: true` or a `ca_file` on an MQTT broker is a configuration error ("MQTT over TLS is not supported in this build …"). To reach a TLS-only MQTT broker, run a TLS-terminating proxy (e.g. stunnel or an Envoy/HAProxy sidecar) next to Mockingbird and point `url` at it. Kafka and AMQP support TLS as usual. `client_id_prefix` (default `mockingbird`) prefixes the client ids and names each mock's consumer group: `<client_id_prefix>-<mock>` for a mock in workspace `default`, `<client_id_prefix>-<workspace>.<mock>` elsewhere, so the instances of one mock share a Kafka group (and an AMQP queue) while same-named mocks of different workspaces do not. [`docs/examples/mockingbird.yaml`](docs/examples/mockingbird.yaml) lists every key. A broker that cannot be reached is reported by `/readyz` (`degraded`, `mock <name>: broker <broker>: <error>`) and retried; the mocks keep serving. Brokers are shared by all workspaces, and topic, queue and routing-key addresses are not isolated per workspace: a mock in any workspace can publish to, or consume from, an address another workspace's mock uses, so do not treat workspaces as a tenant boundary on a shared broker.

### Mocks with channels

```yaml
mocks:
  order-events:
    contract: fixtures/asyncapi/orders-amqp-2.6.yaml
    broker: rabbit              # default: the one broker that serves every channel
    channels:                   # by channel id; unlisted channels keep the contract's settings
      orderCreated:
        triggers:
          - every: 30s
      orderCancelled:
        enabled: false          # stop producing (or consuming) on this channel
```

Without `broker`, the mock uses the single configured broker that serves all its broker channels; if there is none, or more than one, `/readyz` says so and the broker channels stay inactive until the mock's broker is set (in the file, or with `mockingbird channels broker <mock> <broker>`). WebSocket channels need no broker.

### Triggers

A channel the mock publishes generates a message whenever one of its triggers fires:

| Config file | CLI (`--trigger`) | Contract / Admin API (JSON) | Fires |
| --- | --- | --- | --- |
| `every: 30s` | `every:30s` | `{"schedule": {"every_ms": 30000}}` | on a schedule, at least every 100 ms |
| `cron: "0 9 * * 1-5"` | `cron:0 9 * * 1-5` | `{"cron": {"expr": "0 9 * * 1-5"}}` | on a 5-field (or 6-field, with seconds) cron expression, UTC |
| `on_state_change: {resource_type: orders, kind: upserted}` | `state:orders:upserted` | `{"on_state_change": {"resource_type": "orders", "kind": "upserted"}}` | when a resource of that type is upserted, deleted or either (`any`, the default) in the mock's state; the message prompt gets the changed resource and up to 5 more of its type |
| `manual` | `manual` | `"manual"` | only on demand (the Admin API or `mockingbird channels publish`) |

A channel with no triggers is `manual`. Schedule and cron triggers run on one instance at a time (a store-backed lease, 30 s TTL); every channel the mock publishes can also be published on demand.

### Contract extensions

Put these on an operation (2.x `publish`/`subscribe`, 3.0 operations):

- `x-mockingbird-triggers`: a list of triggers in the JSON form above, for channels the mock publishes:

  ```yaml
  x-mockingbird-triggers:
    - schedule: {every_ms: 5000}
    - on_state_change: {resource_type: orders}
  ```

- `x-mockingbird-reply`: on a channel the mock subscribes to, the id of a channel the mock publishes; every valid consumed message is answered with a generated message on it. A 3.0 operation's own `reply` takes precedence.

  **Reply-to addresses.** A 3.0 `reply` may give `address: {location: "$message.header#/<name>"}` (the `address_header` of the compiled reply). The reply then goes where the consumed message's `<name>` header says (matched case-insensitively), instead of the reply channel's own address: on Kafka the header value is the topic, on MQTT the topic (MQTT 5 `response_topic` arrives as a header of that name), and on AMQP it is the routing key on the default exchange (`""`, direct), i.e. the name of the queue to answer, as with AMQP `reply_to`. The binding's other settings (partitions, QoS, retain) are kept; a WebSocket reply always goes back to the sending client. Without the header, or with an empty value, the reply channel's address (with its `{parameters}` generated) is used. Correlation headers (any name containing `correlation id`, ignoring case, `-` and `_`) are copied onto the reply. Only `$message.header` locations are supported; `$message.payload` locations and 2.x documents (which have no `reply`) use the reply channel's address. The address header is taken as given: it is not validated against the reply channel's address template.
- `x-mockingbird-state`: on a channel the mock subscribes to, `{resource_type, id_field, delete}`; every valid consumed message is upserted (or, with `delete: true`, deleted) as the resource whose id is the message's `id_field`, in the session named by its `x-mockingbird-session` header (default `default`).

### Headers and limits

Every produced message carries these headers (written lowercase):

| Header | Value |
| --- | --- |
| `x-mockingbird-source` | `llm`, `repaired` or `fallback` (faker) |
| `x-mockingbird-mock` | the mock's name |
| `x-mockingbird-hop` | the reply depth, `0` for a message that is not a reply |

A reply carries the consumed message's hop plus one, and Mockingbird does not reply to a message whose hop is 3 or more, so two mocks answering each other cannot loop forever. Messages are at most 1 MiB, produced or consumed: a larger generated message is not sent, and a larger consumed message is recorded as truncated and not validated. A publish is retried 3 times (after 200 ms, 400 ms and 800 ms); broker connects and publish attempts time out after 10 s each; producers are flushed for up to 5 s on shutdown.

Every produced and consumed message appears in the mock's request log (`kind: event`, method `PUBLISH` or `RECEIVE`, the channel id as operation id, the resolved address as path) and in the `mb_broker_messages_total{mock, channel, direction, outcome}` metric. Consumed messages are recorded with their validation result (`422` and the errors when they do not match the schema).

### Admin API and CLI

Paths below are relative to `/api/v1/workspaces/{ws}` (and, for workspace `default`, to the deprecated `/api/v1` aliases), except `/api/v1/brokers`:

| Endpoint | Role | Meaning |
| --- | --- | --- |
| `GET /mocks/{mock}/channels` | viewer | the channels with their effective settings and activity |
| `GET /mocks/{mock}/channels/{channel}` | viewer | one channel with its message, key, header and parameter schemas |
| `PUT /mocks/{mock}/channels/{channel}` | editor | `{"enabled": true, "triggers": [...]}`; `triggers: null` restores the contract's (audited as `channel.update`) |
| `POST /mocks/{mock}/channels/{channel}/publish` | editor | `{"count": 3, "context": {...}}`: generate and publish now, 1–100 messages (audited as `channel.publish` with the requested and published counts and the context's top-level keys, never its values; a publish that fails partway is still audited, and its error says how many messages went out) |
| `GET/PUT /mocks/{mock}/event-settings` | viewer / editor | `{"broker": "rabbit"}`; `null` picks automatically (`PUT` audited as `event_settings.update`) |
| `GET /api/v1/brokers` | any authenticated caller | the configured brokers and their status (global, read-only) |

Settings of mocks declared in the config file are read-only through the API (409). The `channels` and `brokers` CLI commands use the `default` workspace paths.

```sh
mockingbird channels list orders
mockingbird channels get orders orderCreated
mockingbird channels set orders orderCreated --trigger every:10s --trigger state:orders
mockingbird channels set orders orderCancelled --disable
mockingbird channels publish orders orderCreated --count 3
mockingbird channels broker orders rabbit       # `-` picks automatically again
mockingbird brokers list
```

### WebSocket channels

A `ws` channel is served on the data plane at `<prefix><path>`, the mock's route prefix followed by the channel's address. Messages the mock publishes go to every client connected to that path; messages clients send are consumed like broker messages, and their replies go back to the sending client only.

WebSocket channels are admitted like a call of every other protocol: the upgrade first goes through tenant resolution and the quota (a rejection is answered with its status, e.g. `429` with `Retry-After`, and recorded as a refused `event` call), and each reply generated for a client's frame is admitted again against the tenancy resolved at the upgrade (a refused reply is not sent, and the received message's record notes `rejected`) and metered to that tenant. They honour the mock's `data_plane_auth` too: in `api_key` mode the upgrade request needs a data-plane key of the mock's workspace in the key header (default `x-api-key`) or the `api_key` query parameter; without one it is answered `401` (with a `WWW-Authenticate` challenge) and recorded in the mock's request log as a refused `event` call. Event records (produced, consumed and refused) belong to the mock's workspace and are listed under `/api/v1/workspaces/{ws}/mocks/{mock}/requests?kind=event`.

```sh
mockingbird serve --offline fixtures/asyncapi/chat-ws-2.6.yaml   # channel /chat, mounted at /
websocat ws://localhost:8080/chat
```

### Images

The official image is built two ways:

| Image | Build | Brokers |
| --- | --- | --- |
| full (default) | `docker build -t mockingbird .` | Kafka, AMQP, MQTT, memory, WebSocket channels |
| slim | `docker build --build-arg FEATURES=--no-default-features -t mockingbird:slim .` | memory and WebSocket channels only |

A plain `cargo build` includes MQTT and AMQP (pure Rust). Kafka is opt-in: `cargo build --release --features full` (or `--features kafka`) compiles librdkafka and OpenSSL from source (both linked statically), which needs `cmake` (3.x), a C/C++ toolchain, `perl`, `make`, `zlib1g-dev` and `libcurl4-openssl-dev` (librdkafka 2.12 includes the curl headers even though it is built without curl). Declaring a broker whose kind the binary was built without is a startup error that names the kind and the fix (`brokers.<name>: this build has no <kind> support (use the full image or build with --features <kind>)`). Channels whose protocol no configured broker of a compiled kind serves stay inactive and are reported by `/readyz`.

### Known limits

- Payloads are always JSON-encoded; Avro schemas are used for generation and validation, but Avro binary encoding is not produced.
- MQTT subscriptions are not shared across instances: every instance consumes every message.
- Kafka GSSAPI (Kerberos) authentication is unsupported; use PLAIN or SCRAM.
- MQTT over TLS is unsupported (see [Brokers](#brokers)); use a TLS-terminating proxy or plain MQTT.
- State-change triggers fire on the instance that observed the change.
- The console Channels tab comes in P6; until then use the Admin API and the CLI.

## Scenarios

A scenario is a named set of overrides for a mock: extra LLM instructions, a forced status, added latency, fields forced into the body (`set`), the item count and pacing of streamed responses (`stream_items`, `stream_item_interval`), optionally limited to some operation ids. Select one per request:

```sh
curl -i localhost:8080/orders/orders/o1 -H 'X-Mockingbird-Scenario: out-of-stock'
```

A scenario's status applies only to operations whose contract declares that status (or a `default` response), where its `set` fields are forced into that response's body too; a 5xx also applies to operations that do not declare it and is answered natively, as with `X-Mockingbird-Status`. Other operations are served normally, with the scenario's other overrides. The Admin API rejects (`422 Scenario status not declared`), and a config file may not declare, a non-5xx status that none of the scenario's operations declares in the mock's active contract, since it could never take effect. An unknown scenario name is a 400. Scenarios come from the config file or from `PUT /api/v1/mocks/{mock}/scenarios/{name}`.

A mock's `default_scenario` applies when a request names none, so clients that cannot send headers still get it. A scenario's `operations` entries may be `{id, instructions}` to give one operation its own instructions in place of the scenario's.

## Admin API

The Admin API lives under `http://127.0.0.1:9090/api/v1`; its OpenAPI document is served at [`/api/openapi.json`](http://127.0.0.1:9090/api/openapi.json). Every error is an RFC 9457 `application/problem+json` document.

| Resource | Endpoints |
| --- | --- |
| Mocks | `GET/POST /mocks`, `GET/PATCH/DELETE /mocks/{mock}` |
| Contracts | `GET/POST /mocks/{mock}/contracts`, `GET …/contracts/{version}`, `GET …/{version}/source` (an `application/octet-stream` attachment; the sniffed type is in `X-Mockingbird-Source-Type`), `POST …/{version}/activate` |
| Scenarios | `GET /mocks/{mock}/scenarios`, `PUT/DELETE …/scenarios/{name}` |
| LLM profiles | `GET /llm-profiles`, `GET/PUT/DELETE /llm-profiles/{name}` |
| Sessions | `GET /mocks/{mock}/sessions`, `GET/PUT/DELETE …/sessions/{session}` (export, seed, reset) |
| Request log | `GET /mocks/{mock}/requests` |
| Imports | `GET/POST /mocks/{mock}/imports`, `GET/DELETE …/imports/{job}` (see [Smart import](#smart-import)) |
| Drafts | `GET/PUT …/contracts/{version}/draft`, `POST …/{version}/approve` (activates unless `{"activate": false}`), `POST …/{version}/preview` |
| Channels and brokers | `GET /mocks/{mock}/channels`, `GET/PUT …/channels/{channel}`, `POST …/{channel}/publish`, `GET/PUT /mocks/{mock}/event-settings`, `GET /brokers` (see [Events](#events)) |

The request log keeps the newest `observability.request_log_retention` calls per mock. Keys matching `observability.redact_fields` are redacted anywhere in recorded JSON bodies, form bodies and query strings: a key matches when, lowercased and stripped of `_`, `-` and `.`, it contains a field normalized the same way (so `token` also redacts `access_token` and `refresh_token`, and `apiKey` redacts `x-api-key`; it errs towards hiding, e.g. `tokenizer_version`); multipart bodies are not recorded, a request body over 64 KiB is replaced by `{"truncated": true, "bytes": N}`, and a response body over 64 KiB by `{"truncated": true, "bytes": N, "preview": "…"}` (the first 1 KiB of the redacted JSON). Redaction and truncation run on the background writer, never on the request path. Queued entries are written before a graceful shutdown completes (for up to 5 s).

Changes take effect on the data plane without a restart: activating a contract version or editing a mock swaps its runtime in place.

If a change breaks a mock (for example an LLM profile it uses now names a secret that is not set), the mock keeps serving its previous version, and `/readyz` reports `degraded` with the reason `mock <name>: <error>` until it is fixed. `PUT` and `DELETE` of an LLM profile still succeed in that case, and list the affected mocks in `warnings`. Two mocks may not share a route prefix: creating or moving a mock onto another mock's exact prefix is a 409.

### Admin port safety

Besides authentication (see [Console and auth](#console-and-auth)), the admin port defends itself against browsers and pasted configs:

- **Host check.** While the admin port is bound to a loopback address (the default), it answers only requests whose `Host` is `localhost`, `127.0.0.1` or `[::1]` (any port); in `oidc` mode it answers only to the host of `auth.external_url`. Anything else gets `421 Misdirected Request`, except the ops endpoints. This blocks DNS-rebinding attacks from web pages.
- **Content types.** Every mutating request needs `Content-Type: application/json` (activation too, whose body is ignored), and a contract upload needs `application/json`, `application/yaml`, `application/x-yaml`, `text/yaml`, `application/vnd.oai.openapi` (`+json`/`+yaml`), `application/graphql`, `text/x-protobuf`, `application/x-protobuf`, `application/xml`, `text/xml`, `application/wsdl+xml`, `application/zip`, `application/gzip` or `application/octet-stream`; anything else is a `415`. A cross-site form or no-cors request cannot send these, so a web page cannot change mocks behind your back. The CLI client sets them for you; with curl, pass `-H 'content-type: application/json'` (or `application/yaml` for a YAML contract).
- **Secret references.** An LLM profile created or updated through the API may only use secret references matching `server.api_secret_refs` (globs; default `["env:MB_LLM_*", "env:*_API_KEY"]`); `file:` references and other variables are rejected with a `422 Secret reference not allowed` whose detail lists the accepted forms (for the defaults: `secret_ref not allowed by server.api_secret_refs; accepted: env:MB_LLM_* or env:*_API_KEY (* matches any characters)`). Profiles in the config file are not restricted. Only a sysadmin may set or change a profile's `secret_ref`, or change the `base_url` of a profile that has one (`403` otherwise): a workspace admin can still create profiles without a secret and change a profile's other fields, but cannot point a secret at a host of their choosing. Widen the list in `mockingbird.yaml` if you need to:

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

### Archive uploads

A contract that spans several files (`.proto` files importing each other, a WSDL with its XSDs, a GraphQL schema split into files) is uploaded as one zip or tar.gz archive. `mockingbird import` packs a directory for you:

```sh
mockingbird import --as orders protos/          # packs protos/ as tar.gz
mockingbird import --as calc calculator.zip
```

An archive is at most 20 MiB, holds at most 500 files and unpacks to at most 64 MiB. It is unpacked in memory only: absolute paths, `..`, links and devices are rejected, and when every file shares one top-level directory, that directory is stripped. Nested archives are not supported. The CLI picks the upload's media type from the file extension (`.graphql`, `.proto`, `.pb`/`.desc`, `.wsdl`/`.xsd`/`.xml`, `.zip`, `.tgz`/`.tar.gz`, `.json`, `.yaml`).

## Smart import

Not every API comes with a contract. Smart import turns what you do have into a **draft** contract version that you review before it serves anything:

| Input | `kind` | How it is read | What it produces |
| --- | --- | --- | --- |
| Markdown, HTML, PDF (at most 500 pages) or plain text documentation | `markdown`, `html`, `pdf`, `text` | Split into chunks of about 6000 tokens (400 overlapping) along headings; an LLM extracts the operations of each chunk; the results are merged and deduplicated. Needs an LLM profile. | Operations with summaries, parameters, request and response schemas, and examples quoted from the text. |
| A HAR 1.2 recording (browser dev tools, proxies) | `har` | Deterministic: the JSON traffic of the most common origin; static assets and other origins are dropped; ids in paths become parameters (`/pets/42` → `/pets/{petId}`). | Operations with schemas inferred from the recorded bodies and up to two examples each. |
| A Postman v2.0 or v2.1 collection | `postman` | Deterministic: every request, folders as tags, saved responses as examples, `:var` and `{{var}}` path segments as parameters. | As for HAR. |
| An http(s) URL to any of the above | (`url`) | Fetched once (see [URL fetching](#url-fetching)), then read according to its content type. | As above. |
| Up to 20 URLs, or up to 100 Markdown, HTML or text documents | (`urls`, `documents`) | Each source as above, under one token budget; operations of all sources are merged into one draft. A source that cannot be fetched or read is skipped with a `source_skipped` warning. | One draft. |

HAR and Postman imports do not need an LLM. When one is available it only writes missing summaries. Passwords, tokens, API keys, cookies, `Authorization` headers and similar values in recordings are replaced by `<redacted>` before anything is stored. Every input is at most 20 MiB.

Every draft is an OpenAPI 3.1 document compiled by the same importer as an uploaded contract, so a draft is always a valid contract. OpenAPI, GraphQL, protobuf and WSDL files are contracts already: upload them to `…/contracts` as before.

### Embedded OpenAPI

Many documentation pages carry their own OpenAPI definition: a fenced ```` ```yaml ```` or ```` ```json ```` block, a `<script type="application/json">` blob, or the page data of a ReadMe site (`script#ssr-props`, `script#__NEXT_DATA__`, `data-initial-props`, including definitions stored as JSON strings such as `oasDefinition`). An OpenAPI 3.x definition found there is taken as written: its operations (with every operation of the definition, not only the one the page describes), components and servers go into the draft with confidence 1.0 and the provenance `embedded OpenAPI`, and the block is removed from the text the LLM reads. When the definitions cover every endpoint the page names, no LLM is called (and none is needed). On a page with both, the block wins: the LLM only fills in operations without a block. A block operation that does not compile is skipped with an `invalid_operation` warning; a block that does not parse stays in the text for the LLM. A Swagger 2.0 block is not converted: it stays in the text, with an `embedded_swagger2` warning.

### Merge or replace

An import **merges** into the mock's newest draft or active version when it has one: new operations are added, an operation with the same method and path (parameter names aside) is replaced, and nothing is removed. Base components win over incoming ones with the same name (a differing one gives a `conflicting_component` warning), and an incoming operation id already used by a kept operation gets a `_2` suffix (the original goes in `x-original-operation-id`). Re-importing the same documentation therefore gives the same operations and ids. `mode: replace` makes the draft hold only what the import found. When the base is not an OpenAPI 3 document (a GraphQL or protobuf mock, say), a default import is stored unmerged with a `merge_skipped` warning, while an explicit `mode: merge` fails the job. Two imports running at once each merge into the same base; the later draft does not include the earlier one's operations.

### Draft quality

- Object schemas get `required`: the properties present in every example, or, without examples, those the documentation marks required.
- A property is nullable (`"type": ["string", "null"]`) when an example shows it as `null` or the documentation says it may be null or is optional. This applies to schemas the LLM wrote as well as inferred ones.
- The documentation's own operation ids are kept; an id that is not a valid identifier is converted (`movie-details` → `movie_details`, camel case kept) and the original stays in `x-original-operation-id`.
- A `default` the documentation does not state verbatim is dropped (`invented_default`).
- Operations below 0.4 confidence are left out, each with a `low_confidence` warning saying why.

### Review flow

1. **Import.** `POST …/imports` queues a job; a pool of in-process workers runs it (`queued` → `running` → `done`, `failed` or `canceled`), reporting the phase (`fetching`, `extracting`, `analyzing`, `summarizing`, `assembling`) and chunk progress. A `done` job names the new contract version in `result_version`, with status `draft`. A draft never serves traffic.
2. **Edit.** `GET …/contracts/{version}/draft` returns the document, each operation with its confidence and provenance, the warnings and a diff against the active version. `PUT …/draft` replaces the document; it must compile, otherwise a `422` lists each problem with a JSON pointer. Edited operations are marked `edited`. `POST …/preview` generates a sample response for one operation without touching any session state (it spends LLM tokens).
3. **Approve.** `POST …/approve` turns the draft into an ordinary version and activates it; it can no longer be edited. With `{"activate": false}` it stays `approved` (the console approves this way and offers activation as its own step).
4. **Activate.** `POST …/activate`, as for any version, swaps the mock to it.

Every step is audited: `import.create` and `import.cancel` (target `import:<mock>/<job>`), `import.complete` (target `contract:<mock>@<version>`, actor kind `system`), `contract.draft_update`, `contract.approve` and `contract.activate`.

### Confidence and provenance

Each operation carries a **confidence** between 0 and 1. Operations seen in HAR or Postman traffic or taken from an embedded OpenAPI block get 1.0 (HAR and Postman 0.8 when a path parameter was inferred from a slug-like segment). Document operations get the LLM's own estimate (0.5 when it gives none), less 0.2 when no response shape was found, and halved when a literal path segment does not occur in the chunk it came from; below 0.4 they are left out of the draft. The console shows it as High, Medium or Low.

**Provenance** says where an operation came from: up to three entries, each with a location (the chunk and heading, or the HAR entry / Postman item) and an excerpt of at most 300 characters. For documents the excerpt is quoted from the source text, so you can check each operation against the documentation.

### Warnings

A draft lists what the importer was unsure about. Warnings never fail a job; check them before you approve.

| Code | Meaning |
| --- | --- |
| `chunk_skipped` | The LLM call for a chunk failed or answered something unusable; that part of the document contributed nothing. |
| `invalid_operation` | The LLM proposed operations without a valid method or path, or an operation (also one from an embedded block) kept the draft from compiling; they were dropped. |
| `invalid_schema` | A schema the LLM proposed does not compile; it was dropped and a schema inferred from the examples was used. |
| `ungrounded_operation` | A path segment of an operation does not occur in the text it supposedly came from; its confidence was halved. Check that it exists. |
| `token_budget_exhausted` | The job's token budget ran out: the draft is **partial** and covers only the chunks processed so far. |
| `conflicting_schemas` | Several sources describe the same operation differently; the richer schema was kept. |
| `no_responses` | No response was described; a bare `200` was added. |
| `missing_path_parameter` | A `{param}` in the path was not described; it was added as a string. |
| `entries_skipped` | HAR entries (static assets, non-JSON traffic, unreadable entries) or Postman items without a usable method or URL were skipped. |
| `other_origins` | The HAR file recorded several origins; only the most common one was imported. |
| `summaries_skipped` | Summaries for HAR or Postman operations could not be generated. |
| `low_confidence` | An operation's confidence was below 0.4, so it was left out; the message gives the reason. |
| `invented_default` | `default` values the documentation does not state were dropped. |
| `schema_too_large` | A response schema is larger than the prompt budget (64 KiB of compact JSON); the generation prompt shows a trimmed schema. |
| `embedded_swagger2` | A Swagger 2.0 block was found; its operations were read from the text instead. |
| `source_skipped` | One source of a several-source import could not be fetched or read. |
| `merge_skipped` | The import could not merge into the mock's version (not OpenAPI 3, or the merged draft did not compile); the draft holds only this import. |
| `conflicting_component` | A component of the import differs from the version's component of the same name; the version's was kept. |

### Token budget

The LLM calls of one job may spend at most `import.max_tokens` tokens (default 200 000), counted from the provider's reported usage. Before each chunk the importer estimates its cost (the chunk, the prompt and the profile's `max_tokens`); when the budget cannot cover it, the job stops early and assembles a partial draft with a `token_budget_exhausted` warning instead of failing. Extraction calls use the mock's LLM profile (or `import.llm_profile`) with a timeout of at least 120 s and at least 8192 output tokens, at temperature 0 and in JSON-object mode (the schema is in the prompt). Document text is passed to the model only as delimited data, never as instructions; the model has no tools, and every answer is validated before use. With `--offline` there is no LLM, so only HAR and Postman imports work.

Imports and draft previews go through the same extension points as data-plane calls. Creating an import job is admitted by the quota guard (operation `import`, the caller's workspace); a refusal is answered with its own status, `429` by default, and `Retry-After` when the guard sets one. Every LLM call an import makes is reported to the usage sink (operation and source `import`, status `502` for a failed call), whether the job finishes, fails or is canceled, and `mb_import_tokens_total` counts the tokens of all of them. Previews are admitted and reported as calls of the previewed operation. `import.llm_profile` is passed through the provider policy like a mock's own profile.

### Configuration

The `import:` section (workers, the per-job token budget, the extraction profile, the private-URL allowlist and the per-mock job limit) is described in the [configuration reference](docs/configuration.md#import). It is read at startup.

### URL fetching

URL imports are fetched by the server, so they are guarded against server-side request forgery:

- Only `http` and `https` URLs with a host and without credentials.
- The host is resolved once and every address is checked before connecting; the connection is pinned to the checked addresses, so the name cannot be re-resolved elsewhere. Blocked: `0.0.0.0/8`, `10.0.0.0/8`, `100.64.0.0/10`, `127.0.0.0/8`, `169.254.0.0/16`, `172.16.0.0/12`, `192.0.0.0/24`, `192.0.2.0/24`, `192.168.0.0/16`, `198.18.0.0/15`, `198.51.100.0/24`, `203.0.113.0/24`, `224.0.0.0/4`, `240.0.0.0/4`, and for IPv6 `::`, `::1`, `fc00::/7`, `fe80::/10`, `fec0::/10`, `ff00::/8` and `2001:db8::/32` (IPv4-mapped and NAT64 addresses are judged by their IPv4 address).
- `import.url_allowlist` exempts a host name, a host suffix (`*.corp.example`) or an address range. A blocked fetch fails with a generic "destination not allowed" message that names neither the host nor the address it resolved to, so URL imports cannot probe internal DNS; the server logs both at `warn` level.
- Query values and the fragment are redacted (`?sig=REDACTED`) in everything stored or audited: the job's `source_name`, the draft's `source_name` and `source_url`, and the `import.create` audit event. The real URL is used only for the fetch.
- At most 5 redirects, and each hop is validated and resolved again, exactly like the first request.
- No proxy is used (proxy environment variables are ignored), the whole fetch takes at most 20 s and the body is at most 10 MiB.
- The content type picks the reader: `text/html`, `text/markdown`, `text/plain`, `application/pdf`, or JSON that is a HAR file or a Postman collection.

### Admin API

| Endpoint | Role | Result |
| --- | --- | --- |
| `POST /mocks/{mock}/imports` | Edit | `202` with the job and a `Location` header. `409 managed_by_file` for a mock from the config file; `422` for an unknown kind, a contract, or a document import without an LLM; `429` when the mock already has `import.max_active_jobs_per_mock` queued or running jobs. |
| `GET /mocks/{mock}/imports`, `GET …/imports/{job}` | Read | The newest 50 jobs; one job. Finished jobs are kept for 7 days. |
| `DELETE …/imports/{job}` | Edit | Cancels a queued or running job; `409` once it has finished. |
| `GET …/contracts/{version}/draft`, `PUT …/draft` | Read, Edit | The draft for review; replace its document (`409` unless the version is a draft). |
| `POST …/contracts/{version}/approve` | Edit | The approved version, activated unless the body is `{"activate": false}` (the body may be empty or `{}`). |
| `POST …/contracts/{version}/preview` | Edit | A generated response for one operation. |

The JSON body takes exactly one of `url`; `urls` (1 to 20 URLs); `kind` with `text` (`markdown`, `html`, `text`, `har`, `postman`); `content_base64` with an optional `kind` and `filename`; or `documents` (1 to 100 of `{filename?, kind?, text}`, Markdown, HTML or text, kind from `kind` or the file name, at most 20 MiB in all). `mode` is `merge` or `replace` (absent: merge when the mock has a draft or active version). The draft's `meta` names its `sources` (several-source imports) and the version it was `merged_from`. A file can also be uploaded as `multipart/form-data` with a `file` part and an optional `kind` part; the request must carry an `X-Mockingbird-Upload` header (any value), which a cross-site form cannot send. The request body is at most 28 MiB.

```sh
ADMIN=http://127.0.0.1:9090/api/v1/workspaces/default/mocks/pets
# Upload a file.
curl -X POST "$ADMIN/imports" -H 'X-Mockingbird-Upload: 1' -F file=@petstore.har
# Or send JSON.
curl -X POST "$ADMIN/imports" -H 'content-type: application/json' \
  -d '{"url": "https://example.com/docs"}'
curl -X POST "$ADMIN/imports" -H 'content-type: application/json' \
  -d "$(jq -Rs '{kind: "markdown", text: .}' petstore.md)"
# Several pages into one draft, replacing the current contract.
curl -X POST "$ADMIN/imports" -H 'content-type: application/json' \
  -d '{"urls": ["https://example.com/docs/pets", "https://example.com/docs/owners"], "mode": "replace"}'
# Follow the job, then review the draft it names.
curl "$ADMIN/imports/<job id>"
curl "$ADMIN/contracts/<version>/draft"
```

### CLI

`mockingbird import` recognises documentation, HAR files and Postman collections (by content, and by file name for Markdown, HTML and text), http(s) URLs (several at once), and folders holding only `.md`, `.markdown`, `.html`, `.htm` and `.txt` files (imported as documentation, one source per file; any other folder uploads as an archive). It creates the mock if needed, waits for the job (`--timeout`, default 10 minutes) and prints the draft's operations and warnings:

```sh
mockingbird import petstore.har --as pets
mockingbird import https://example.com/docs/pets https://example.com/docs/owners --as pets
mockingbird import ./docs --as pets --mode replace
# Approve (and activate) in one go, but only if the draft has no warnings.
mockingbird import petstore.har --as pets --approve
```

With `--approve`, a draft with warnings is left unapproved: the command prints the summary, says how many warnings there are and exits 1, so you can review it in the console. An approved draft is activated unless `--no-activate`; `--activate` is still accepted with `--approve`. `--mode merge|replace` picks the [merge mode](#merge-or-replace).

### Console

A mock's **Import** tab starts an import from a file, a URL or pasted text, and follows the job's progress. The **draft review** page (opened from a draft on the Contracts tab, or when an import finishes) lists the operations with their confidence and provenance, edits an operation, previews a response, shows the diff against the active version and the warnings, and approves and activates the draft.

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
  oidc:
    issuer: https://auth.example.com/auth/v1/      # exactly the discovery `issuer` (Rauthy's ends in /)
    client_id: mockingbird
    client_secret: env:MB_OIDC_CLIENT_SECRET       # a secret reference
```

Every `auth`, `auth.oidc` and `audit` key is in the [configuration reference](docs/configuration.md#auth).

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

A call without a valid, unrevoked `data-plane` key of the mock's workspace (or a sysadmin key) gets `401`. The key is never passed to the engine or recorded. `mode: passthrough_validate` instead requires credentials in the form the contract's security requirements declare, without verifying them against any issuer (only OpenAPI and Swagger contracts declare any).

Data-plane auth applies to every protocol: a GraphQL request is admitted (tenant, quota) and then answered `401` before it executes, and the refusal is logged; a GraphQL subscription connection is authenticated once, by the key header or `?api_key=` on the upgrade request (an invalid key is a `401` with no upgrade) or by the `connection_init` payload (the key header name, `api_key` or `apiKey`), and is otherwise refused with close code `4401` (`graphql-transport-ws`) or a `connection_error` (`graphql-ws`); the payload's credential members are never forwarded or logged. A gRPC call reads the key from its request metadata (e.g. `x-api-key: mbk_…`) and fails with `UNAUTHENTICATED`, and a SOAP call (and `GET ?wsdl`) reads it from its HTTP headers and gets a `401` client Fault (`401` problem for the WSDL). The `?api_key=` parameter works for REST, SSE and GraphQL over HTTP and WebSocket.

### Workspaces

Every resource lives in a workspace: `/api/v1/workspaces/{ws}/mocks…`, `…/llm-profiles…`, `…/api-keys…` and `…/audit`. `GET /api/v1/workspaces` lists them, `POST` creates one (sysadmin), and `GET /api/v1/me` shows the caller's roles. Workspace names follow the mock-name rule (`^[a-z0-9][a-z0-9-]{0,62}$`). The `default` workspace always exists and cannot be deleted. Route prefixes stay unique across all workspaces. A mock in the config file picks its workspace with `workspace:` (default `default`).

The P2 paths `/api/v1/mocks…` and `/api/v1/llm-profiles…` still work and act on `default`. They are **deprecated** (and marked so in the OpenAPI document); use the workspace paths.

### Audit log

Every successful change writes one audit event: the actor, the action (`mock.create`, `contract.upload`, `session.reset`, `api_key.create`, `auth.login`, …), the target, and before/after snapshots. Events never contain secrets or tokens. Read them in the console (**Audit**), with `GET /api/v1/workspaces/{ws}/audit` (filters `action`, `actor`, `before`, `limit`), or for all workspaces with `GET /api/v1/audit` (sysadmin). Add `format=jsonl` to export them as JSON Lines. Events older than `audit.retention_days` are pruned daily. Failed console logins (`auth.login_failed`) are audited at most once per source address per 10 s and 100 times per minute overall; the rest only count in `mb_auth_login_failed_suppressed_total`.

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

Open http://localhost:9090 and log in as `demo@localhost.test` / `Demo-Passw0rd-123!`. **The stack's secrets are published dev values: use it only on your own machine.** See [Docker Compose](docs/deploy/compose.md) for the HA and broker profiles.

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
| `/readyz` | Readiness, with the node's `role` and whether it is the `leader`: 503 `unavailable` when the control store is unreachable; otherwise 200 with `ready`, or `degraded` plus `reasons` (state store or bus failing, a mock that failed to rebuild, a broker down, a daily token budget spent). In `auth.mode: oidc` the reasons are shown only to callers with valid credentials. |
| `/metrics` | Prometheus metrics. |

The data plane keeps `/__mockingbird/healthz`. Set `OTEL_EXPORTER_OTLP_ENDPOINT` to export traces over OTLP (HTTP/protobuf). The [runbook](docs/operations/runbook.md) explains each `/readyz` reason and the metrics to alert on.

## Production

- **Storage.** SQLite is the default (in memory, or `sqlite://<path>`). For shared or long-lived deployments use Postgres (`--storage-url postgres://…` or `MB_STORAGE_URL`); migrations run at startup under an advisory lock. Session state uses the same database unless `storage.state_url` names another (`memory`, Postgres or Redis).
- **High availability.** `--role control` and `--role data` split the Admin API and console from the mocks, over a Postgres or Redis bus with leader election. See [architecture](docs/architecture.md#high-availability) and [Kubernetes](docs/deploy/kubernetes.md).
- **Hardening.** Global and per-mock limits (concurrency, request and response size, rate limit, daily token budget), per-mock chaos, the output PII scrubber, TLS termination and outbound CA and proxy settings are all in the [configuration reference](docs/configuration.md).

To run the Postgres-gated tests against a throwaway local server: `scripts/test-postgres.sh`; the Redis-gated ones: `scripts/test-redis.sh`. Otherwise they read `MB_TEST_POSTGRES_URL` and `MB_TEST_REDIS_URL` and skip when unset.

## Docker

```sh
docker build -t mockingbird .
docker run --rm mockingbird --version
```

This builds the full image (Kafka, AMQP and MQTT); `--build-arg FEATURES=--no-default-features` builds the slim one (see [Images](#images)). Release images are published as `ghcr.io/rj-s-org/mockingbird:<version>` (and `-slim`), signed with cosign; see [verifying releases](docs/deploy/kubernetes.md#verifying-images-and-binaries).

The image builds the console too and exposes ports 8080 (data plane), 9090 (Admin API, console and ops) and 50051 (gRPC). It runs as a non-root user and, like the binary, binds loopback unless told otherwise. With `auth.mode: none`, binding all interfaces needs `--insecure-no-auth`; only do this on a trusted network (or use `oidc`, as in the [Compose stack](deploy/compose/README.md)):

```sh
docker run --rm -p 8080:8080 -p 9090:9090 -p 50051:50051 \
  -v "$PWD/orders.yaml:/work/orders.yaml:ro" \
  mockingbird serve --host 0.0.0.0 --insecure-no-auth --offline /work/orders.yaml
```

## Reference

`mockingbird serve --help` lists the flags, and the [configuration reference](docs/configuration.md#environment-variables-and-flags) maps every flag and `MB_*` variable to its key. The CLI client reads `MB_ADMIN_URL` (default `http://127.0.0.1:9090`), `MB_ADMIN_TOKEN` and `MB_WORKSPACE`.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.
