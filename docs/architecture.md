# Architecture

Mockingbird turns an API contract into a running mock. Each call is decoded against the contract, answered by an LLM within the contract's response schema, validated, repaired once if needed, and replaced by a schema faker when generation fails or is refused. This page describes how the pieces fit together and how a multi-node deployment stays consistent.

## Crates and boundaries

The library crates live in `crates/mb-*` and are named `mb_*` in code; on crates.io they are published as `mockingbird-<name>` (`mb-engine` → `mockingbird-engine`) for `mockingbird-cli`, with no API stability guarantee. `mb-ext` is the exception: the public extension API, published under its own name and semver.

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
| `mb-evals` | Weekly LLM evals and the offline throughput bench (not published). |
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
   A 2xx answer to a command with a stored target or related resources is asked for as `{"response": <the answer>, "effects": [{"type", "id", "set"}]}`, in the prompt and, under `output_mode: json_schema`, in the response format (this replaces the `{"items": [...]}` wrapping of an array answer, and is left out when the answer itself has a `response` or `effects` member). A plain answer is accepted as it is. Only the answer is validated, patched and repaired; the effects, at most 10, get the PII scrubber and count towards the size cap, where one that does not fit is dropped and the answer kept. Malformed, surplus and oversized effects, and all those of an answer the provider cut at `max_tokens`, count as `rejected`.
6. **Validate → repair → fallback** (`mb.validate`, `mb.repair`). Values breaking a format the faker produces are refilled locally (never sent for repair), and errors a local fix can solve are patched, also inside an `anyOf` or `oneOf` branch whose `const` and `enum` members already hold. One repair call with the validation errors; then the faker, whose answer is flagged in its fallback reason (and `mb_faker_invalid_total`) when no draw validates. The response carries `X-Mockingbird-Source: llm | patched | repaired | fallback`.
7. **Scrub.** With `pii.enabled`, personal-data look-alikes are replaced with synthetic values of the same shape (`mb_pii_replacements_total{pattern}`). Scrubbed output still validates.
8. **State write and encode** (`mb.encode`). Commands upsert or delete the resource and publish a state change, then apply the answer's effects (`mb_effects_total{mock,outcome}`; each is checked and upserted like a write, sends `<resource>.updated` to matching webhook targets, and is kept in the request record's `request.effects` with its outcome; none under `consistency: off`); the adapter encodes the response. Streams (`drop_stream_rate`) may be cut short by chaos.

A request failing validation never reaches the LLM: it is answered with the contract's 400, 422 or `default` error shape (else problem+json), with source `rejected`, whose message-like field, the `X-Mockingbird-Validation` header and the request record's `request.validation` carry the reasons. Form and multipart bodies and query strings are decoded with bracket notation (`a[b][c]=1`, `a[0]=x`, `a[]=y`) and coerced through the schema before validation.

Every call is recorded in the request log (redacted) and counted in `mb_requests_total{mock,operation,status,source}`. Its `source` is `llm`, `patched`, `repaired`, `fallback`, `state`, `cache` or `rejected` (a request that failed validation; as in `X-Mockingbird-Source`), or `not_generated` for a call answered without generating a body: refused before generation (auth, quota, rate limit, an invalid override, an unknown scenario) or answered with a native error (a forced or injected error status the contract declares no response for).

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

With Postgres, leadership can briefly overlap. The lock is released the moment the holder's session ends on the server (a terminated backend, a server restart, a dropped connection), so another node may acquire it at once, while the old holder only notices at its next renew (a `SELECT 1` every 5 s, itself timing out after 5 s). For up to one renew interval, and up to 10 s when the connection hangs rather than fails, both nodes may run leader work. Reconcile writes the same desired state from either node and the retention jobs are idempotent deletes, so an overlap costs duplicate work and, at worst, a logged conflict.

The bus and the leader lock need a session-level Postgres connection: a pooler in transaction mode (PgBouncer `pool_mode = transaction`, and statement mode) is unsupported for them, because `LISTEN/NOTIFY` and session advisory locks do not survive a connection being handed to another client between transactions. Point `bus.url` (or, without one, `storage.url`) at Postgres directly or through a session-mode pooler, or use Redis for the bus.

### Bus capacity and resync

Each node fans bus traffic out through in-process broadcast channels of 1024 events. Besides the control events (a mock, profile or workspace changed), the same channels carry the per-request traffic: state changes for subscriptions and change feeds, and request-log notices for the console's live view. A busy data plane can therefore fill them; a receiver that falls behind skips the missed events and, rather than guess what it lost, triggers a full local `Resync`.

Every `Resync`, whether from lag, a reconnect, the networked bus's safety resync (about every 5 minutes) or another node's request, rebuilds every mounted mock from the control store. Rebuilding a mock resets its in-memory rate-limit buckets (callers briefly get a full burst again) and restarts its event channels: their triggers and consumers start over, and WebSocket channel subscribers are disconnected and must reconnect. GraphQL subscriptions and SSE streams already open stay on the previous build of the mock (its contract and settings at the time) until the client reconnects. `mb_bus_events_total{type="resync"}` counts them; a steady rate means the node is lagging and should be scaled out.

### Per-mock scheduler lease

Channel triggers (`every`, `cron`, `on_state_change`) run on data-plane nodes, so each mock's scheduler is guarded by its own lease in the `scheduler_leases` table (30 s): only the holder fires, and another node takes over when it expires. Consumers share the work through broker features instead: one Kafka consumer group and one AMQP queue per mock, and MQTT shared subscriptions (`$share/<client_id_prefix>/<topic>`) when the bus is shared.

## State backends

| Profile | Control store | Session state | Bus |
| --- | --- | --- | --- |
| Local | SQLite (file or memory) | SQLite or memory | In-process |
| HA | Postgres | Postgres or Redis | Postgres `LISTEN/NOTIFY` or Redis pub/sub |

Session state is keyed by workspace, mock, session, resource type and id, and expires `storage.session_ttl` after its last write. `mb_state_sessions{mock}` counts live sessions on every data-serving node.

Each node also keeps a per-session response cache in memory (500 answers per session, least recently used first out), so a session repeating a read gets the same answer (`X-Mockingbird-Source: cache`). A write of a resource type, from this node or relayed over the bus, drops that type's answers; a session reset or seed drops the session's, and a mock reset every session's of that mock, on every node (the node that made it announces it on the bus); an idle session's answers expire with `storage.session_ttl`. Mocks with `consistency: off` bypass the cache.

The Redis state store needs `maxmemory-policy noeviction`: evicting a key would silently lose a session's resources. It uses multi-key transactions without hash tags, so Redis Cluster is not supported; use a single primary.

Migrations run at startup under an advisory lock, for SQLite and Postgres alike. See [upgrading](upgrading.md) for the migration history.
