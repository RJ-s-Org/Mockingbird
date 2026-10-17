# Architecture

Mockingbird turns an API contract into a running mock. Each call is decoded against the contract, answered by an LLM within the contract's response schema, validated, repaired once if needed, and replaced by a schema faker when generation fails or is refused. This page describes how the pieces fit together and how a multi-node deployment stays consistent.

## Crates and boundaries

| Crate | Role |
| --- | --- |
| `mb-mcm` | The Mockingbird Contract Model: operations, channels, per-operation JSON Schemas. No internal dependencies; every other crate exchanges MCM types. |
| `mb-import` | Deterministic compilers (OpenAPI 3, Swagger 2, GraphQL, protobuf, AsyncAPI, WSDL) and the LLM-assisted draft importers (documentation, HAR, Postman). Runs on the control plane only. |
| `mb-llm` | The provider trait and the OpenAI-compatible client (structured output, streaming, outbound CA/proxy). |
| `mb-engine` | Prompt building, generation, validate → repair → fallback, the schema faker, the PII scrubber and the engine guards. No protocol or transport knowledge, and no `axum`. |
| `mb-state` | Session state stores (memory, SQLite, Postgres, Redis), the cross-node bus and leader election. No `axum`. |
| `mb-store` | The control-plane store (SQLite, Postgres) and its migrations. No `axum`. |
| `mb-protocols` | Protocol adapters behind Cargo features: REST, GraphQL, gRPC, SOAP, SSE, WebSocket, Kafka, AMQP, MQTT. Admission limits and chaos. Depends on neither `mb-store` nor `mb-auth`. |
| `mb-ext` | The extension contract (tenancy, quota, usage, secrets, provider policy, admin hooks) with self-hosted defaults. Semver-stable from 1.0; see [its README](../crates/mb-ext/README.md). |
| `mb-auth` | OIDC relying party, RBAC and API keys. |
| `mb-admin` | The Admin REST API (OpenAPI-described) and the audit log. |
| `mb-console` | The embedded React console. |
| `mb-evals` | Nightly LLM evals and the offline throughput bench (not published). |
| `mockingbird` | The binary: CLI, configuration, role wiring, HA bridge, budget, TLS. |

Protocol adapters contain no generation logic: they decode traffic into an operation call and encode the generated response (or stream items) back onto the wire. Data-plane processes load compiled MCM bundles from the control-plane store and never parse source contracts on the request path.

## Roles

A node runs one role (`server.role`, `MB_ROLE` or `--role`):

| | `all` | `control` | `data` |
| --- | --- | --- | --- |
| Data listener and gRPC listener | yes | not bound | yes |
| Admin port | Admin API, console, ops | Admin API, console, ops | ops only (`/healthz`, `/readyz`, `/metrics`) |
| Reconcile, `--watch` | leader only | leader only | never; `--watch` is a usage error |
| Maintenance (session expiry, audit and token-usage retention) | leader only | leader only | no |
| Smart import workers | yes | yes | no |
| Mock runtimes | full | built for manual publish only; channel runtimes start no consumers, triggers or WebSocket routes | full |

`all` is the default and needs nothing shared: SQLite and in-memory state work. `control` and `data` share their stores with other nodes, so they need a Postgres control store and a Postgres or Redis state store; startup refuses SQLite or `memory`.

A data node's Admin port authenticates with API keys only (it needs no OIDC discovery). Manual publish on a WebSocket channel from a control node answers 409, since WebSocket channels are served by data nodes.

A typical HA layout is two `control` replicas (one leader at a time) and an autoscaled set of `data` replicas behind a load balancer, sharing Postgres and, optionally, Redis.

## Request flow

1. **Decode.** The adapter matches the call to an operation and validates its input. Invalid input gets the contract's declared 4xx or error shape, built without an LLM call.
2. **Admission** (`mb.admission` span): the request size cap (`limits.max_request_bytes`, or the mock's own; 413, recorded), data-plane auth, the `mb-ext` tenant resolver and quota guard, then the mock's rate limit (429 with `Retry-After`, `mb_rate_limited_total`).
3. **Plan and chaos.** Override headers and the active scenario are applied. When the mock (or the active scenario) has chaos and the call does not carry `X-Mockingbird-Chaos: off` with header overrides enabled: latency jitter is added, and with probability `error_rate` the call is answered with an injected error in the contract's error shape (or the protocol's native error), marked `X-Mockingbird-Chaos: error`. Counted in `mb_chaos_injected_total{mock,kind}`.
4. **State lookup** (`mb.state`). The session (`X-Mockingbird-Session`, cookie or gRPC metadata; `default` otherwise) and related resources. When the state store fails, the mock serves statelessly and `/readyz` reports `degraded`.
5. **Prompt** (`mb.prompt`) and **generate** (`mb.llm`), inside the engine guards:
   - the global generation semaphore (`limits.max_concurrent_generations`, `mb_generation_in_flight`); a call that waits longer than its LLM timeout falls back (`mb_generation_queue_timeouts_total`);
   - the mock's daily token budget (`mb_token_budget_used`, `mb_token_budget_exhausted_total`), shared across replicas through the `token_usage` table and flushed every 5 s (the leader deletes rows older than 90 days, daily);
   - the response size cap (`limits.max_response_bytes`).
   A refused guard never errors: the faker answers.
6. **Validate → repair → fallback** (`mb.validate`, `mb.repair`). One repair call with the validation errors; then the faker. The response carries `X-Mockingbird-Source: llm | repaired | fallback`.
7. **Scrub.** With `pii.enabled`, personal-data look-alikes are replaced with synthetic values of the same shape (`mb_pii_replacements_total{pattern}`). Scrubbed output still validates.
8. **State write and encode** (`mb.encode`). Commands upsert or delete the resource and publish a state change; the adapter encodes the response. Streams (`drop_stream_rate`) may be cut short by chaos.

Every call is recorded in the request log (redacted) and counted in `mb_requests_total{mock,operation,status,source}`.

## High availability

### The bus

Nodes tell each other what changed over a bus (`bus.url`): Postgres `LISTEN/NOTIFY` on channel `mockingbird_bus`, Redis pub/sub on channel `mockingbird:bus`, or in-process for one node. Payloads are a JSON `BusEnvelope { origin, event }` that carries ids only; receivers reload from the stores. Events:

| Event | Sent when | Receivers |
| --- | --- | --- |
| `Mock { workspace, name }` | a mock, its contract, scenarios, chaos or limits change | rebuild that mock's runtime |
| `Profile { workspace, name }` | an LLM profile changes | rebuild the mocks that use it |
| `Workspace { name }` | a workspace changes | reload its mocks |
| `State { workspace, mock, session, resource_type, id, deleted }` | a session resource changes | fire `on_state_change` triggers and SSE/subscription updates |
| `RequestLogged { id }` | a call is recorded | the live tail looks the entry up by id |
| `Resync` | locally after a reconnect and every 5 minutes (±25 %) as a safety net; to the other nodes once the bus works again after a failed publish; instead of an event larger than 7900 bytes | reload every mock |

A connection that goes half-open (a partition, or conntrack or a load balancer dropping an idle flow) never reports an error by itself, so each node heartbeats its subscriber connection every 10 s (`SELECT 1` on the Postgres listener, `PING` on the Redis pub/sub connection) and treats a heartbeat that fails or takes over 5 s as a lost connection. A disconnected bus reconnects with a backoff from 1 s doubling to 30 s, and `/readyz` reports `degraded` with `bus: <error>` meanwhile. Because every reconnect emits `Resync`, a node never keeps serving a deleted mock for longer than one heartbeat timeout plus one reconnect cycle. `mb_bus_connected` and `mb_bus_events_total{direction,type}` expose the bus.

### Leader election

Work that must run once per cluster is leader-gated: the startup reconcile of `mockingbird.yaml`, `--watch`, and maintenance (session expiry, audit and token-usage retention). The leader holds the lock `control`:

- with Postgres, a session advisory lock whose key is the first 8 bytes (big-endian `i64`) of sha256(`"mockingbird:control"`);
- with Redis, the key `mockingbird:leader:control`, a lease of 15 s renewed every 5 s;
- with an in-process bus, the node always leads.

A node that loses its connection stops leader work within 5 s, and another takes over within 15 s. `mb_leader` is 1 on the leader, and `/readyz` shows `"role"` and `"leader"`.

### Per-mock scheduler lease

Channel triggers (`every`, `cron`, `on_state_change`) run on data-plane nodes, so each mock's scheduler is guarded by its own lease in the `scheduler_leases` table (30 s): only the holder fires, and another node takes over when it expires. Consumers share the work through broker features instead: one Kafka consumer group and one AMQP queue per mock, and MQTT shared subscriptions (`$share/<client_id_prefix>/<topic>`) when the bus is shared.

## State backends

| Profile | Control store | Session state | Bus |
| --- | --- | --- | --- |
| Local | SQLite (file or memory) | SQLite or memory | In-process |
| HA | Postgres | Postgres or Redis | Postgres `LISTEN/NOTIFY` or Redis pub/sub |

Session state is keyed by workspace, mock, session, resource type and id, and expires `storage.session_ttl` after its last write. `mb_state_sessions{mock}` counts live sessions on every data-serving node.

The Redis state store needs `maxmemory-policy noeviction`: evicting a key would silently lose a session's resources. It uses multi-key transactions without hash tags, so Redis Cluster is not supported; use a single primary.

Migrations run at startup under an advisory lock, for SQLite and Postgres alike. See [upgrading](upgrading.md) for the migration history.
