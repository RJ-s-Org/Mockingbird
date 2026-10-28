# Events

An AsyncAPI contract makes a mock event-driven: each channel operation becomes a *channel* that Mockingbird either publishes generated messages on or consumes, validates and records messages from. Channels run next to the mock's other protocols and share its session state, LLM profile and persona.

## AsyncAPI support

AsyncAPI 2.0–2.6 and 3.0, in JSON or YAML (detected by the `asyncapi:` key), with `$ref`s inside the document. Channel and operation bindings for `kafka` (topic, key schema), `amqp` (exchange or queue, routing key) and `mqtt` (topic, QoS, retain) are read, and `ws` channels are served over WebSocket; without a binding, the protocol of the channel's server is used. Message payloads may be JSON Schema or Avro (`schemaFormat: application/vnd.apache.avro…`), which is converted to JSON Schema. Message headers, Kafka keys and `{parameter}`s in addresses are generated from their schemas.

AsyncAPI describes operations from the application's point of view, which is the point of view of the service Mockingbird stands in for:

| AsyncAPI 2.x | AsyncAPI 3.0 | Mockingbird |
| --- | --- | --- |
| `subscribe` (clients receive) | `action: send` | **publishes** generated messages |
| `publish` (clients send) | `action: receive` | **subscribes**: consumes, validates and records messages, optionally replying or changing state |

The channel id is the operation id (`operationId` in 2.x, the operation key in 3.0).

## Brokers

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

`kind` is `kafka`, `amqp`, `mqtt` or `memory`. Passwords are secret references in `sasl.password`, never part of a URL. MQTT brokers are reached over plain TCP only: this build's MQTT client (rumqttc) is compiled without its TLS stack, whose `rustls-webpki` 0.102 has open security advisories, so an `mqtts://` or `ssl://` URL, `tls: true` or a `ca_file` on an MQTT broker is a configuration error ("MQTT over TLS is not supported in this build …"). To reach a TLS-only MQTT broker, run a TLS-terminating proxy (e.g. stunnel or an Envoy/HAProxy sidecar) next to Mockingbird and point `url` at it. Kafka and AMQP support TLS as usual. `client_id_prefix` (default `mockingbird`) prefixes the client ids and names each mock's consumer group: `<client_id_prefix>-<mock>` for a mock in workspace `default`, `<client_id_prefix>-<workspace>.<mock>` elsewhere, so the instances of one mock share a Kafka group (and an AMQP queue) while same-named mocks of different workspaces do not. [`docs/examples/mockingbird.yaml`](examples/mockingbird.yaml) lists every key. A broker that cannot be reached is reported by `/readyz` (`degraded`, `mock <name>: broker <broker>: <error>`) and retried; the mocks keep serving. Brokers are shared by all workspaces, and topic, queue and routing-key addresses are not isolated per workspace: a mock in any workspace can publish to, or consume from, an address another workspace's mock uses, so do not treat workspaces as a tenant boundary on a shared broker.

## Mocks with channels

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

## Triggers

A channel the mock publishes generates a message whenever one of its triggers fires:

| Config file | CLI (`--trigger`) | Contract / Admin API (JSON) | Fires |
| --- | --- | --- | --- |
| `every: 30s` | `every:30s` | `{"schedule": {"every_ms": 30000}}` | on a schedule, at least every 100 ms |
| `cron: "0 9 * * 1-5"` | `cron:0 9 * * 1-5` | `{"cron": {"expr": "0 9 * * 1-5"}}` | on a 5-field (or 6-field, with seconds) cron expression, UTC |
| `on_state_change: {resource_type: orders, kind: upserted}` | `state:orders:upserted` | `{"on_state_change": {"resource_type": "orders", "kind": "upserted"}}` | when a resource of that type is upserted, deleted or either (`any`, the default) in the mock's state; the message prompt gets the changed resource and up to 5 more of its type |
| `manual` | `manual` | `"manual"` | only on demand (the Admin API or `mockingbird channels publish`) |

A channel with no triggers is `manual`. Schedule and cron triggers run on one instance at a time (a store-backed lease, 30 s TTL); every channel the mock publishes can also be published on demand.

## Contract extensions

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

## Headers and limits

Every produced message carries these headers (written lowercase):

| Header | Value |
| --- | --- |
| `x-mockingbird-source` | `llm`, `repaired` or `fallback` (faker) |
| `x-mockingbird-mock` | the mock's name |
| `x-mockingbird-hop` | the reply depth, `0` for a message that is not a reply |

A reply carries the consumed message's hop plus one, and Mockingbird does not reply to a message whose hop is 3 or more, so two mocks answering each other cannot loop forever. Messages are at most 1 MiB, produced or consumed: a larger generated message is not sent, and a larger consumed message is recorded as truncated and not validated. A publish is retried 3 times (after 200 ms, 400 ms and 800 ms); broker connects and publish attempts time out after 10 s each; producers are flushed for up to 5 s on shutdown.

Every produced and consumed message appears in the mock's request log (`kind: event`, method `PUBLISH` or `RECEIVE`, the channel id as operation id, the resolved address as path) and in the `mb_broker_messages_total{mock, channel, direction, outcome}` metric. Consumed messages are recorded with their validation result (`422` and the errors when they do not match the schema).

## Admin API and CLI

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

## WebSocket channels

A `ws` channel is served on the data plane at `<prefix><path>`, the mock's route prefix followed by the channel's address. Messages the mock publishes go to every client connected to that path; messages clients send are consumed like broker messages, and their replies go back to the sending client only.

WebSocket channels are admitted like a call of every other protocol: the upgrade first goes through tenant resolution and the quota (a rejection is answered with its status, e.g. `429` with `Retry-After`, and recorded as a refused `event` call), and each reply generated for a client's frame is admitted again against the tenancy resolved at the upgrade (a refused reply is not sent, and the received message's record notes `rejected`) and metered to that tenant. They honour the mock's `data_plane_auth` too: in `api_key` mode the upgrade request needs a data-plane key of the mock's workspace in the key header (default `x-api-key`) or the `api_key` query parameter; without one it is answered `401` (with a `WWW-Authenticate` challenge) and recorded in the mock's request log as a refused `event` call. Event records (produced, consumed and refused) belong to the mock's workspace and are listed under `/api/v1/workspaces/{ws}/mocks/{mock}/requests?kind=event`.

```sh
mockingbird serve --offline fixtures/asyncapi/chat-ws-2.6.yaml   # channel /chat, mounted at /
websocat ws://localhost:8080/chat
```

## Images

The official image is built two ways:

| Image | Build | Brokers |
| --- | --- | --- |
| full (default) | `docker build -t mockingbird .` | Kafka, AMQP, MQTT, memory, WebSocket channels |
| slim | `docker build --build-arg FEATURES=--no-default-features -t mockingbird:slim .` | memory and WebSocket channels only |

A plain `cargo build` includes MQTT and AMQP (pure Rust). Kafka is opt-in: `cargo build --release --features full` (or `--features kafka`) compiles librdkafka and OpenSSL from source (both linked statically), which needs `cmake` (3.x), a C/C++ toolchain, `perl`, `make`, `zlib1g-dev` and `libcurl4-openssl-dev` (librdkafka 2.12 includes the curl headers even though it is built without curl). Declaring a broker whose kind the binary was built without is a startup error that names the kind and the fix (`brokers.<name>: this build has no <kind> support (use the full image or build with --features <kind>)`). Channels whose protocol no configured broker of a compiled kind serves stay inactive and are reported by `/readyz`.

## Known limits

- Payloads are always JSON-encoded; Avro schemas are used for generation and validation, but Avro binary encoding is not produced.
- MQTT subscriptions are not shared across instances: every instance consumes every message.
- Kafka GSSAPI (Kerberos) authentication is unsupported; use PLAIN or SCRAM.
- MQTT over TLS is unsupported (see [Brokers](#brokers)); use a TLS-terminating proxy or plain MQTT.
- State-change triggers fire on the instance that observed the change.
- The console Channels tab comes in P6; until then use the Admin API and the CLI.
