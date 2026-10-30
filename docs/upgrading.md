# Upgrading

Mockingbird is 0.x: minor releases may change behaviour, and each one is described here and in the [changelog](../CHANGELOG.md). The `mb-ext` crate is versioned on its own and is semver-stable from 1.0.

For the mechanics of a zero-downtime upgrade (control nodes first, then data nodes), see [rolling upgrades](operations/runbook.md#rolling-upgrades).

## Migrations

Migrations run at startup under an advisory lock, for SQLite and Postgres alike, and are forward-only: back up the control store before upgrading (see [backup and restore](operations/runbook.md#postgres-backup-and-restore)), and do not start an older binary against a migrated database.

| Migration | Release | Change |
| --- | --- | --- |
| `0001_init` | P2 | Mocks, contract versions, scenarios, LLM profiles, request log. |
| `0002_workspaces` | P3 | Workspaces and per-mock data-plane auth. |
| `0003_auth_audit` | P3 | API keys, admin sessions and the audit log. |
| `0100_events` | P5 | `request_log.kind`, channel and event settings, scheduler leases. |
| `0200_smart_import` | P6 | `contract_versions.draft_meta` and import jobs. |
| `0300_hardening` | P7 | `mocks.chaos`, `mocks.limits`, the `token_usage` table. |
| `0900_admin_session_grants` | Unreleased | Nullable `admin_sessions` columns (`claims`, `mapping_hash`, `refresh_token`, `refreshed_at`) so console roles can be re-derived after login. Additive; sessions from before keep their login roles. |
| `1000_profile_output_mode` | Unreleased | Nullable `llm_profiles.output_mode` (`none`, `json_object`, `json_schema`). Additive; a profile written before reads its mode from `json_schema`, which is still written, so older nodes keep working during a rolling upgrade. |

### `0300_hardening` is additive

`0300_hardening` only adds: a nullable `mocks.chaos` JSON column, a `mocks.limits` JSON column that defaults to `{}`, and the table `token_usage(mock_id, day, tokens)`. No existing column changes, and stored JSON from earlier releases reads back unchanged, so:

- it runs quickly on a large database (no table rewrite);
- nodes of the previous release keep working against the migrated schema during a rolling upgrade (they ignore the new columns);
- no data needs converting. Existing mocks have no chaos and no limits until you set them.

## Upgrading to 0.1.0

### Relative paths in the config file follow the file

In a config file, relative `sqlite:` paths in `storage.url` and `storage.state_url`, and relative `server.tls.cert_file`, `server.tls.key_file`, `outbound.ca_file` and `brokers.<name>.ca_file`, now resolve against the config file's directory, like `mocks.<name>.contract` and `proxy.ca_dir` already did. Before, they resolved against the working directory.

Nothing changes when you start Mockingbird from the config file's directory (`mockingbird run`, `serve -c mockingbird.yaml`), which is what `mockingbird init` sets up. If you start it from elsewhere with `-c some/dir/mockingbird.yaml` and `storage.url: sqlite://./mockingbird.db`, it now opens `some/dir/mockingbird.db` instead of `./mockingbird.db`: move the database next to the config, or write an absolute path. `--storage-url`, `MB_STORAGE_URL`, `MB_STATE_URL` and runs without a config file still resolve against the working directory.

## Upgrading to the P7 release

New, all off or neutral by default:

- **Roles** (`server.role`, `--role`, `MB_ROLE`). The default `all` is the previous behaviour. `control` and `data` need Postgres storage and Postgres or Redis state.
- **The bus** (`bus.url`, `MB_BUS_URL`). With Postgres storage, nodes notify each other of changes over `LISTEN/NOTIFY` by default, so every node reloads a changed mock. Nothing to configure.
- **Leader election.** With a shared bus, only the leader reconciles `mockingbird.yaml` and runs maintenance (session expiry, audit and token-usage retention). The result is the same as before; the work is just not repeated on every node.
- **Redis** session state (`storage.state_url: redis://…`). It needs `maxmemory-policy noeviction`.
- **Limits** (`limits:` and per-mock `limits`), **chaos** (per mock and per scenario), the **PII scrubber** (`pii:`), **TLS** termination (`server.tls`) and outbound **CA/proxy** settings (`outbound:`).

Changes that may need attention:

- **Request size cap.** Requests over 1 MiB (`limits.max_request_bytes`) are now answered 413. Raise the limit if your tests send larger bodies.
- **Response size cap.** Generated responses over 1 MiB (`limits.max_response_bytes`) are replaced by faker output.
- **Generation concurrency.** At most 64 LLM generations run at once per node (`limits.max_concurrent_generations`); calls beyond that wait up to their LLM timeout and then fall back to the faker.
- **GraphQL caps.** Operations may select at most 20 root fields, a WebSocket connection may hold at most 16 subscriptions and a mock at most 1000 connections (`protocols.graphql.*`).
- **`--watch` on a data node** is a usage error.
- **Range checks.** `protocols.stream.item_interval` and a scenario's `stream_item_interval` must be at most 30 s, now also through the Admin API.
- **TLS and `auth.external_url`.** With `server.tls`, `auth.external_url` must be `https://`, in every auth mode.

## `mb-ext` 1.0 (extension authors)

`mb-ext` 1.0 freezes the extension API. Code written against the 0.x crate needs these changes:

- **Constructors instead of struct literals** for the types the core builds and hands to you, which are now `#[non_exhaustive]`:
  - `CallInfo::new(mock, operation_id, headers)`;
  - `UsageEvent::new(tenancy, mock, operation_id)` (status 0, empty source, zero latency and tokens; the fields are still public, so assign the rest);
  - `Extensions::default()` plus the builders `with_tenants`, `with_quota`, `with_usage`, `with_secrets`, `with_providers` and `with_admin_hooks`, each replacing one extension:

    ```rust
    let ext = Extensions::default()
        .with_quota(Arc::new(MyQuota::new()))
        .with_usage(Arc::new(MyUsage));
    ```

- **`match` on `ExtError` needs a wildcard arm**: it is `#[non_exhaustive]` and gained no variant you must handle (`SecretNotFound`, `BadSecretRef`, `Other`).
- **New helpers:** `Rejection::new(status, title, detail)` with `.with_retry_after(duration)`, and `Tenancy::new(tenant, workspace)`. `Rejection` and `Tenancy` stay exhaustive, so struct literals keep working.
- **Secrets:** `Extensions::default()` now uses `DefaultSecrets`, which adds `vault:<mount>/<path>#<field>` references when `VAULT_ADDR` is set. A custom `SecretStore` is unaffected.

The full stability policy is in the [`mb-ext` README](../crates/mb-ext/README.md). [`examples/custom-ext`](../examples/custom-ext) is a complete binary with a custom tenant resolver and quota built on 1.0.
