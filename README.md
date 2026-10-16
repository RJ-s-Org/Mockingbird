# Mockingbird

A mock-endpoint platform: import an API contract and get a running mock that answers with schema-valid, LLM-generated responses.

## Quickstart

Install from a checkout:

```sh
cargo install --path crates/mockingbird
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

Every response carries `X-Mockingbird-Source: llm | repaired | fallback`.

Check a contract without serving it:

```sh
mockingbird validate orders.yaml
```

Mockingbird listens on three ports: the data plane (REST, SSE, GraphQL and SOAP mocks) on `127.0.0.1:8080`, gRPC mocks on `127.0.0.1:50051` (h2c), and the Admin API plus ops endpoints on `127.0.0.1:9090`. If you did not choose an admin port (`--admin-port`, `MB_ADMIN_PORT` or `server.admin_port`) and 9090 is already taken, Mockingbird logs a warning and serves the data plane without the admin port. The gRPC port (`--grpc-port`, `MB_GRPC_PORT` or `server.grpc_port`) works the same way: when 50051 is taken, gRPC mocks are not served. A port you chose explicitly must be free.

## Config file

For anything beyond a quick look, declare the server, storage, LLM profiles and mocks in `mockingbird.yaml`:

```yaml
server:
  port: 8080
  admin_port: 9090
  grpc_port: 50051
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
      firehose:
        stream_items: 20              # items per streamed response
        stream_item_interval: 500ms   # pause between streamed items
protocols:                            # every key is optional; these are the defaults
  graphql:
    path: /graphql                    # below each GraphQL mock's route prefix
    introspection: true
    max_depth: 15
    max_complexity: 1000
  stream:
    default_items: 5                  # items per streamed response
    max_items: 50                     # upper bound, whatever a request asks for
    item_interval: 0s                 # pause before every item but the first
  sse:
    keepalive: 15s                    # interval between keepalive comments
  grpc:
    max_message_bytes: 4194304        # 4 MiB, decoding and encoding
```

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

Brokers are declared once in the config file and read at startup:

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

`kind` is `kafka`, `amqp`, `mqtt` or `memory`. Passwords are secret references in `sasl.password`, never part of a URL. [`docs/examples/mockingbird.yaml`](docs/examples/mockingbird.yaml) lists every key. A broker that cannot be reached is reported by `/readyz` (`degraded`, `mock <name>: broker <broker>: <error>`) and retried; the mocks keep serving.

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
| `POST /mocks/{mock}/channels/{channel}/publish` | editor | `{"count": 3, "context": {...}}`: generate and publish now, 1–100 messages (audited as `channel.publish`) |
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

WebSocket channels honour the mock's `data_plane_auth` like every other protocol: in `api_key` mode the upgrade request needs a data-plane key of the mock's workspace in the key header (default `x-api-key`) or the `api_key` query parameter; without one it is answered `401` (with a `WWW-Authenticate` challenge) and recorded in the mock's request log as a refused `event` call. Event records (produced, consumed and refused) belong to the mock's workspace and are listed under `/api/v1/workspaces/{ws}/mocks/{mock}/requests?kind=event`.

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

A plain `cargo build` includes MQTT and AMQP (pure Rust). Kafka is opt-in: `cargo build --release --features full` (or `--features kafka`) compiles librdkafka from source, which needs `cmake` (3.x), a C/C++ toolchain, `libssl-dev` and `zlib1g-dev`. Declaring a broker whose kind the binary was built without is a startup error that names the kind and the fix (`brokers.<name>: this build has no <kind> support (use the full image or build with --features <kind>)`). Channels whose protocol no configured broker of a compiled kind serves stay inactive and are reported by `/readyz`.

### Known limits

- Payloads are always JSON-encoded; Avro schemas are used for generation and validation, but Avro binary encoding is not produced.
- MQTT subscriptions are not shared across instances: every instance consumes every message.
- Kafka GSSAPI (Kerberos) authentication is unsupported; use PLAIN or SCRAM.
- State-change triggers fire on the instance that observed the change.
- The console Channels tab comes in P6; until then use the Admin API and the CLI.

## Scenarios

A scenario is a named set of overrides for a mock: extra LLM instructions, a forced status, added latency, fields forced into the body (`set`), the item count and pacing of streamed responses (`stream_items`, `stream_item_interval`), optionally limited to some operation ids. Select one per request:

```sh
curl -i localhost:8080/orders/orders/o1 -H 'X-Mockingbird-Scenario: out-of-stock'
```

A scenario's status applies only to operations whose contract declares that status (or a `default` response); other operations are served normally, with the scenario's other overrides. An unknown scenario name is a 400. Scenarios come from the config file or from `PUT /api/v1/mocks/{mock}/scenarios/{name}`.

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
| Channels and brokers | `GET /mocks/{mock}/channels`, `GET/PUT …/channels/{channel}`, `POST …/{channel}/publish`, `GET/PUT /mocks/{mock}/event-settings`, `GET /brokers` (see [Events](#events)) |

The request log keeps the newest `observability.request_log_retention` calls per mock. Keys matching `observability.redact_fields` are redacted anywhere in recorded JSON bodies, form bodies and query strings: a key matches when, lowercased and stripped of `_`, `-` and `.`, it contains a field normalized the same way (so `token` also redacts `access_token` and `refresh_token`, and `apiKey` redacts `x-api-key`; it errs towards hiding, e.g. `tokenizer_version`); multipart bodies are not recorded, a request body over 64 KiB is replaced by `{"truncated": true, "bytes": N}`, and a response body over 64 KiB by `{"truncated": true, "bytes": N, "preview": "…"}` (the first 1 KiB of the redacted JSON). Redaction and truncation run on the background writer, never on the request path. Queued entries are written before a graceful shutdown completes (for up to 5 s).

Changes take effect on the data plane without a restart: activating a contract version or editing a mock swaps its runtime in place.

If a change breaks a mock (for example an LLM profile it uses now names a secret that is not set), the mock keeps serving its previous version, and `/readyz` reports `degraded` with the reason `mock <name>: <error>` until it is fixed. `PUT` and `DELETE` of an LLM profile still succeed in that case, and list the affected mocks in `warnings`. Two mocks may not share a route prefix: creating or moving a mock onto another mock's exact prefix is a 409.

### Admin port safety

Besides authentication (see [Console and auth](#console-and-auth)), the admin port defends itself against browsers and pasted configs:

- **Host check.** While the admin port is bound to a loopback address (the default), it answers only requests whose `Host` is `localhost`, `127.0.0.1` or `[::1]` (any port); in `oidc` mode it answers only to the host of `auth.external_url`. Anything else gets `421 Misdirected Request`, except the ops endpoints. This blocks DNS-rebinding attacks from web pages.
- **Content types.** Every mutating request needs `Content-Type: application/json` (activation too, whose body is ignored), and a contract upload needs `application/json`, `application/yaml`, `application/x-yaml`, `text/yaml`, `application/vnd.oai.openapi` (`+json`/`+yaml`), `application/graphql`, `text/x-protobuf`, `application/x-protobuf`, `application/xml`, `text/xml`, `application/wsdl+xml`, `application/zip`, `application/gzip` or `application/octet-stream`; anything else is a `415`. A cross-site form or no-cors request cannot send these, so a web page cannot change mocks behind your back. The CLI client sets them for you; with curl, pass `-H 'content-type: application/json'` (or `application/yaml` for a YAML contract).
- **Secret references.** An LLM profile created or updated through the API may only use secret references matching `server.api_secret_refs` (globs; default `["env:MB_LLM_*", "env:*_API_KEY"]`); `file:` references and other variables are rejected with `422 secret_ref not allowed by server.api_secret_refs`. Profiles in the config file are not restricted. Only a sysadmin may set or change a profile's `secret_ref`, or change the `base_url` of a profile that has one (`403` otherwise): a workspace admin can still create profiles without a secret and change a profile's other fields, but cannot point a secret at a host of their choosing. Widen the list in `mockingbird.yaml` if you need to:

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
  session_ttl: 12h                                 # absolute console session lifetime (max 30d)
  sysadmin_subjects: ["3f2c…"]                     # OIDC `sub` values that are always sysadmin
  public_metrics: true                             # serve /metrics without credentials
  oidc:
    issuer: https://auth.example.com/auth/v1/      # exactly the discovery `issuer` (Rauthy's ends in /)
    client_id: mockingbird
    client_secret: env:MB_OIDC_CLIENT_SECRET       # env:NAME or file:/path
    scopes: [openid, profile, email, groups]       # default
    roles_claim: groups                            # default
    group_prefix: mb                               # default
    audience: mockingbird                          # `aud` of bearer JWTs; default: client_id
audit:
  retention_days: 365                              # 1..=3650
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
| `/readyz` | Readiness: 503 `unavailable` when the control store is unreachable; otherwise 200 with `ready`, or `degraded` plus reasons when the state store is failing and mocks serve statelessly, or a mock failed to rebuild and serves its previous version. Each store ping is bounded to 2 s. In `auth.mode: oidc` the reasons are shown only to callers with valid credentials; anyone else gets just the status. |
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

This builds the full image (Kafka, AMQP and MQTT); `--build-arg FEATURES=--no-default-features` builds the slim one (see [Images](#images)).

The image builds the console too and exposes ports 8080 (data plane), 9090 (Admin API, console and ops) and 50051 (gRPC). It runs as a non-root user and, like the binary, binds loopback unless told otherwise. With `auth.mode: none`, binding all interfaces needs `--insecure-no-auth`; only do this on a trusted network (or use `oidc`, as in the [Compose stack](deploy/compose/README.md)):

```sh
docker run --rm -p 8080:8080 -p 9090:9090 -p 50051:50051 \
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
| `--host` | `127.0.0.1` | Bind address of all ports. A non-loopback host requires `--insecure-no-auth`. |
| `--port` | `8080` | Data-plane port. |
| `--admin-port` | `9090` | Admin API and ops port. |
| `--grpc-port` | `50051` | gRPC port (h2c), shared by every gRPC mock. |
| `--storage-url` | `sqlite::memory:` | Control-plane storage. |
| `--insecure-no-auth` | off | Permit a non-loopback bind with `auth.mode: none`. |
| `--offline` | off | Faker only; no LLM calls. |
| `--llm-timeout` | `3s` | LLM timeout of the default profile before falling back. |

### Environment

| Variable | Default | Meaning |
| --- | --- | --- |
| `MB_HOST`, `MB_PORT`, `MB_ADMIN_PORT`, `MB_GRPC_PORT`, `MB_STORAGE_URL` | see flags | Same as the flags; flags win. |
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
