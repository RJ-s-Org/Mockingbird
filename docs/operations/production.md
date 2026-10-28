# Running in production

## Ops endpoints

On the admin port:

| Path | Meaning |
| --- | --- |
| `/healthz` | Liveness: the process is up. |
| `/readyz` | Readiness, with the node's `role` and whether it is the `leader`: 503 `unavailable` when the control store is unreachable; otherwise 200 with `ready`, or `degraded` plus `reasons` (state store or bus failing, a mock that failed to rebuild, a broker down, a daily token budget spent). In `auth.mode: oidc` the reasons are shown only to callers with valid credentials. |
| `/metrics` | Prometheus metrics. |

The data plane keeps `/__mockingbird/healthz`. Set `OTEL_EXPORTER_OTLP_ENDPOINT` to export traces over OTLP (HTTP/protobuf). The [runbook](runbook.md) explains each `/readyz` reason and the metrics to alert on.

## Storage, availability and hardening

- **Storage.** SQLite is the default (in memory, or `sqlite://<path>`). For shared or long-lived deployments use Postgres (`--storage-url postgres://…` or `MB_STORAGE_URL`); migrations run at startup under an advisory lock. Session state uses the same database unless `storage.state_url` names another (`memory`, Postgres or Redis).
- **High availability.** `--role control` and `--role data` split the Admin API and console from the mocks, over a Postgres or Redis bus with leader election. See [architecture](../architecture.md#high-availability) and [Kubernetes](../deploy/kubernetes.md).
- **Hardening.** Global and per-mock limits (concurrency, request and response size, rate limit, daily token budget), per-mock chaos, the output PII scrubber, TLS termination and outbound CA and proxy settings are all in the [configuration reference](../configuration.md).

To run the Postgres-gated tests against a throwaway local server: `scripts/test-postgres.sh`; the Redis-gated ones: `scripts/test-redis.sh`. Otherwise they read `MB_TEST_POSTGRES_URL` and `MB_TEST_REDIS_URL` and skip when unset.
