# Operations runbook

One section per signal: what to watch, what it means and what to do. Metrics are on `/metrics` of the admin port. `/readyz` answers `{"status", "role", "leader", "reasons"}`:

- `ready` (200): nothing to report.
- `degraded` (200): the node serves, with the problems listed in `reasons`. The node stays in the load balancer.
- `unavailable` (503): the control store is unreachable (`control store unreachable: <error>`). The node is taken out of the load balancer.

In `auth.mode: oidc`, `reasons` is shown only to callers with valid credentials (an API key works).

Useful queries assume the default Prometheus naming; `X-Mockingbird-Source` is the `source` label of `mb_requests_total`.

## LLM slow or down

**Signal:** a rising fallback ratio, `sum(rate(mb_requests_total{source="fallback"}[5m])) / sum(rate(mb_requests_total[5m]))`; `mb_generation_queue_timeouts_total` increasing; `mb_generation_in_flight` pinned at `limits.max_concurrent_generations`.

**Meaning:** calls are answered by the schema faker instead of the LLM. Responses stay contract-valid (and are tagged `X-Mockingbird-Source: fallback`), so nothing errors, but realism drops. Queue timeouts mean calls waited for a generation slot longer than their LLM timeout: either the provider is slow (each call holds its slot longer) or the concurrency limit is too low for the traffic.

**Action:**
1. Check the provider's status and the `mb.llm` span latency in your traces.
2. If the provider is slow but healthy, raise the profile's `timeout`, or configure a `fallback` profile (another provider or a local model).
3. If the provider is fine and only the queue is full, raise `limits.max_concurrent_generations` (up to 4096) within the provider's rate limits, or add data nodes.
4. While the provider is down, nothing needs doing: traffic keeps being answered.

## Budget exhausted

**Signal:** `/readyz` reason `mock <name>: daily token budget exhausted`; `mb_token_budget_exhausted_total{mock}` increasing; `mb_token_budget_used{mock}` at the mock's `limits.daily_token_budget`.

**Meaning:** the mock spent its LLM tokens for the current UTC day, across every replica. Its calls are served by the faker until 00:00 UTC.

**Action:** if expected, nothing. Otherwise find the caller in the request log (`GET /api/v1/mocks/<mock>/requests`), add a rate limit, or raise the budget (the Admin API's mock `limits`, or `mocks.<name>.limits.daily_token_budget`). The change applies at once; usage already counted stays. Per-day usage is kept 90 days (pruned daily by the leader), so the `token_usage` table stays small.

## Rate limiting

**Signal:** `mb_rate_limited_total{mock}` increasing; clients see 429 with `Retry-After`.

**Meaning:** a mock's `limits.rate_limit` refused calls. The bucket is per mock on each node, so the cluster-wide limit is roughly the per-node limit times the number of data nodes.

**Action:** if the traffic is legitimate, raise `requests_per_second` or `burst`. If one client floods, give it its own mock (or API key and quota through an `mb-ext` `QuotaGuard`).

## State store down (degraded)

**Signal:** `/readyz` reasons `state store unreachable: <error>` and `mock <name>: state store failing, serving statelessly`; `mb_state_errors_total{mock}` increasing.

**Meaning:** session state (Postgres or Redis, `storage.state_url`) cannot be read or written. Mocks keep answering without state: reads do not see earlier writes, and `on_state_change` triggers do not fire. Nothing is lost that was already stored.

**Action:** restore the state store (connectivity, credentials, disk, Redis memory). Nodes recover by themselves on the next successful call; no restart is needed. If Redis evicted keys, see [Redis eviction policy](#redis-eviction-policy).

## Bus down

**Signal:** `/readyz` reason `bus: <error>`; `mb_bus_connected` is 0; `mb_bus_events_total{direction="received"}` flat on data nodes.

**Meaning:** this node misses change notifications, so a mock created, changed or deleted elsewhere is not reloaded here yet. The node keeps serving what it has. The bus reconnects with a backoff of 1 s doubling to 30 s, and every reconnect triggers a `Resync` that reloads all mocks, so nothing stays stale for longer than one reconnect cycle once the bus is back. A link that went half-open without an error is caught by the 10 s heartbeat (5 s timeout), and every node also resyncs about every 5 minutes as a safety net.

**Action:** restore Redis or Postgres (whichever `bus.url`, or `storage.url` when unset, points at). If a node does not reconnect, restart it; it loads every mock at startup. Watch for `mb_bus_events_total{type="resync"}` after recovery.

## Frequent resyncs

**Signal:** `mb_bus_events_total{type="resync"}` rising faster than about once per 5 minutes per node; warnings `bus events lagged; resyncing` or `control changes lagged; resyncing`; WebSocket channel clients report disconnects, or a rate limit lets a burst through.

**Meaning:** per-request bus traffic (state changes behind subscriptions and change feeds, request-log notices for the console) shares each node's broadcast capacity (1024 events) with the control events. When a node falls behind, it cannot tell which control changes it missed, so it runs a full local `Resync`. Every `Resync` rebuilds all mounted mocks: rate-limit buckets start full again, event channels restart (WebSocket channel subscribers are disconnected), and GraphQL subscriptions and SSE streams already open keep the previous build of the mock until the client reconnects. A networked bus also resyncs about every 5 minutes as a safety net, so some resyncs are normal.

**Action:** scale out data nodes or reduce the per-node request rate until lag warnings stop. Make channel clients reconnect on close. Do not rely on a per-node rate limit for an exact count across a resync.

## Leader failover

**Signal:** `mb_leader` (1 on the leader). Alert when `sum(mb_leader)` across control nodes is 0 for more than a minute, or above 1 at all. `/readyz` shows `"leader": true|false`.

**Meaning:** one control (or `all`) node holds the `control` lock and runs the reconcile, `--watch` and maintenance (session expiry, audit and token-usage retention). A leader that loses its store connection stops that work within 5 s; another node takes over within 15 s (the Redis lease TTL; with Postgres, as soon as the advisory lock's session ends).

With a Postgres lock, leadership can overlap: when the holder's session ends on the server, another node can take the lock at once, while the old holder notices only at its next renew, up to one renew interval (5 s) later, or 10 s when its connection hangs. `sum(mb_leader)` can then briefly read 2. Reconcile and retention are idempotent, so this costs duplicate work at worst; alert only when it persists.

**Action:** a short gap is normal during deployments. If no node leads, check the control nodes' connectivity to the lock's backend (Postgres for a Postgres or local bus, Redis for a Redis bus). Leaderless periods only delay reconcile and maintenance; the data plane is unaffected. PgBouncer (or any pooler) in transaction mode is unsupported for the bus and the leader lock: it breaks `LISTEN/NOTIFY` and session advisory locks, so leadership flaps or several nodes lead. Connect `bus.url` to Postgres directly or through a session-mode pool, or use Redis for the bus.

## Broker unavailable

**Signal:** `/readyz` reasons `mock <name>: broker <broker>: <error>` or `mock <name>: channel <id>: <error>`; `mb_broker_messages_dropped_total{kind,address}` increasing.

**Meaning:** a Kafka, AMQP or MQTT connection is down. Producers buffer a little and then drop messages (the counter); consumers stop receiving. HTTP, gRPC and the other mocks are unaffected.

**Action:** check the broker and the `brokers.<name>` settings (address, TLS, SASL secret). Clients reconnect by themselves. Check the broker channel state in the console's Channels tab or `GET /api/v1/mocks/<mock>/channels`.

## Effects rejected

**Signal:** `mb_effects_total{mock,outcome="rejected"}` rising against `outcome="applied"`.

**Meaning:** LLM answers name changes to other resources that fail a check: the resource is not stored in the session, the change touches its identity, the result breaks its schema, its type has no read to give a schema, or the store took no write. The count also covers effects dropped earlier as malformed, past the limit of 10, too large for `limits.max_response_bytes` or in an answer the provider cut at `max_tokens`. Calls are still served, and the rejected change is simply not made, so a later read shows the old value. The request log keeps each effect that reached these checks under the record's `request.effects`, with its `outcome` and `reason`, and each is logged at `info` (`effect of an answer`); effects dropped earlier appear only in the count.

**Action:** none is needed for a few. When they dominate for one mock, read the log lines: a schema the model keeps breaking usually needs a clearer `description` or persona. A burst alongside `mb_state_errors_total{mock}` is the store, not the model.

## Request log dropped

**Signal:** `mb_request_log_dropped_total{mock}` (and `mb_request_log_errors_total`) increasing.

**Meaning:** the request-log writer cannot keep up with traffic (its queue is full) or the control store is rejecting writes. Calls are still served; only their log entries are lost.

**Action:** check control-store latency and errors (`mb_request_log_errors_total`). Under sustained load beyond what the store can write, the drops are the designed back-pressure: the live tail and request log are diagnostic, not an audit trail. Scale the database, or move session state to Redis to take load off it.

## Audit webhook failures

**Signal:** `mb_audit_webhook_failed_total` (a delivery failed after its retries) or `mb_audit_webhook_dropped_total` (the queue was full) increasing.

**Meaning:** some audit events did not reach `audit.webhook_url`. They are still in the audit log in the control store (`GET /api/v1/audit`, kept `audit.retention_days`).

**Action:** fix the receiver (status, TLS: add its CA with `outbound.ca_file`, proxy: `outbound.proxy`), then replay the missing window from the Admin API's audit log. Check that the receiver verifies `X-Mockingbird-Signature` with the current `audit.webhook_secret`.

## Certificate rotation

**Signal:** certificate expiry from your monitoring (for example blackbox exporter `probe_ssl_earliest_cert_expiry`); the log line `cannot reload the TLS certificate; keeping the old one`.

**Meaning:** with `server.tls`, Mockingbird checks `cert_file` and `key_file` every 60 s and loads a changed pair without a restart. If the new pair is invalid (unreadable, mismatched key), the old certificate stays in use and the error is logged on every check until the files are fixed.

**Action:** replace both files (cert-manager, or your secret sync, writes them in place); no restart is needed. Write the key and certificate together, or the reload may see a mismatched pair once and retry a minute later. For outbound CAs (`outbound.ca_file`) and OIDC or broker CA files, a restart is needed.

## Postgres backup and restore

**What to back up:** the control-store database (`storage.url`): mocks, contract versions, scenarios, profiles, API keys (hashed), audit log, request log and token usage. Session state in Postgres is in the same database; it is disposable (it expires after `storage.session_ttl`).

**Backup:** use your platform's backup (managed snapshots or `pg_dump -Fc`). Mockingbird needs no quiescing; a dump is consistent.

**Restore:**
1. Scale every Mockingbird Deployment to 0.
2. Restore the database (`pg_restore --clean --if-exists -d <db> dump.pgc`).
3. Start the control nodes first; migrations bring an older dump up to the running version. Then start the data nodes.
4. Mocks declared in `mockingbird.yaml` are reconciled again by the leader, so the file wins over the restored rows for those.

A restore into a newer Mockingbird is supported; into an older one it is not (migrations are forward-only).

## Redis eviction policy

**Signal:** Redis `evicted_keys` above 0; sessions losing resources at random.

**Meaning:** the session state store requires `maxmemory-policy noeviction`. With any eviction policy, Redis silently deletes session keys under memory pressure. With `noeviction`, a full Redis rejects writes instead, which Mockingbird reports (see [state store down](#state-store-down-degraded)).

**Action:** set `maxmemory-policy noeviction` (`CONFIG SET maxmemory-policy noeviction`, and in the server config), size `maxmemory` for your sessions (`mb_state_sessions{mock}` counts them), and shorten `storage.session_ttl` if needed. Do not share this Redis with a cache that relies on eviction. Redis Cluster is not supported; use a single primary.

## Rolling upgrades

1. Read [upgrading](../upgrading.md) for the target version.
2. **Control nodes first.** The first new node runs migrations at startup under an advisory lock; the others wait for it. Migrations are additive within 0.x, so old data nodes keep working against the new schema.
3. **Then data nodes**, with the usual rolling update (the chart's PodDisruptionBudget keeps one up). Each new node loads every mock at startup.
4. In `all` mode, roll the pods one by one; the first runs the migrations.

Leadership moves during the roll; reconcile and maintenance pause for at most `LEADER_LEASE_TTL` (15 s) at a time.

## Scaling data nodes

Data nodes keep no local state: sessions are in the state store and mocks are loaded from the control store at startup. Scale freely (the chart's HPA does it on CPU). Keep in mind:

- Rate limits are per node: the cluster-wide rate grows with the replica count.
- `limits.max_concurrent_generations` is per node: total concurrent LLM calls grow with replicas, so keep the sum within your provider's limits.
- The daily token budget is cluster-wide, so it does not grow.
- Kafka consumer groups and AMQP queues are shared per mock, and MQTT uses shared subscriptions, so consumers spread across nodes without duplicates. Trigger schedulers run once per mock (per-mock lease).

## Draining for maintenance

1. Stop routing new traffic to the node (cordon and evict on Kubernetes; on Kubernetes 1.30 or newer the pod's 5 s `preStop` sleep lets endpoints drop it; on 1.27 to 1.29 the chart renders no `preStop`, so a request may still reach the pod briefly after SIGTERM: see [graceful shutdown](../deploy/kubernetes.md#graceful-shutdown)).
2. On SIGTERM, Mockingbird stops accepting connections, finishes in-flight calls, stops its leader-only work, flushes queued broker messages and request-log entries (each for up to 5 s), and exits. The chart gives pods 30 s. Its leadership (and any per-mock scheduler lease) passes to another node when the lock or lease expires.
3. Long-lived streams (SSE, WebSocket, gRPC streams) are closed at shutdown; clients should reconnect to another node.
4. Draining the leader hands leadership to another control node within 15 s.
