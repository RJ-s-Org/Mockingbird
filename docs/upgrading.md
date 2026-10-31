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
| `1100_import_job_tenant` | Unreleased | Nullable `import_jobs.tenant`: the tenant an import job is billed to, so whichever node runs it bills that tenant. Additive; a job queued before (or by an older node during a rolling upgrade) has none and is billed to `default`, as before. |

### `0300_hardening` is additive

`0300_hardening` only adds: a nullable `mocks.chaos` JSON column, a `mocks.limits` JSON column that defaults to `{}`, and the table `token_usage(mock_id, day, tokens)`. No existing column changes, and stored JSON from earlier releases reads back unchanged, so:

- it runs quickly on a large database (no table rewrite);
- nodes of the previous release keep working against the migrated schema during a rolling upgrade (they ignore the new columns);
- no data needs converting. Existing mocks have no chaos and no limits until you set them.

## Upgrading past 0.1.0 (unreleased)

### Action routes act on their parent resource

A singular `POST <static>/{param}/<verb>` with no `GET` on its path and no route below it, such as `POST /v1/payment_intents/{intent}/confirm`, used to compile as a create on a resource type named after `<verb>`. It now acts on `<static>`: an update when it answers what `GET <static>/{param}` answers, otherwise a command that stores nothing. Operation ids and routes stay the same. These change:

- its operation's `kind`, `resource_type` and `identity` in the compiled bundle, while `source_digest`, which hashes the source, does not;
- its webhook event type, from `confirm.created` to `payment_intent.updated`, or for a command that answers a record of its own, from `cancel.created` to `booking.updated`;
- what state it writes: resources stored under the old type (`confirm`) in a session or a session snapshot are no longer read by anything, and `on_state_change` triggers on that type stop firing.

A bundle takes this on recompile, and stored bundles are not recompiled on their own. For a mock managed through the API, upload the contract again. For a mock from the config file, restart: with an in-memory control store that is enough, but with a persistent one a mock keeps its active version while the contract's digest matches it, so also change a byte of the contract, such as a comment.

### Resources composed with `allOf` have an identity

A resource whose schema is an `allOf` at its root used to compile with no identity. The importer now reads the members of its `allOf` branches, so every such resource has one on recompile. On those contracts:

- reads by path start serving stored resources, where they were generated knowing nothing stored;
- `unknown_ids: 404` starts answering `404` where it generated;
- webhook and `on_state_change` trigger behaviour follows the identity.

Stored bundles take this the way they take action routes, with the same steps: upload the contract again, or restart a mock from the config file, changing a byte of the contract if the control store is persistent.

### A resource answers only for its owner

On a route that names its target after another path parameter, such as `/repos/{owner}/{repo}` or `/orgs/{org}/teams/{slug}`, a read must now be able to tell a stored resource is the route owner's, unless the parameter is named as an id (`order_id`). It can when the resource was listed or written under the same route template and collection path. Otherwise the resource must show the owner: a relative path ending in the owner and the name (`full_name`, `path_with_namespace`), a member tied to the parameter (`namespace`, `owner.login`, `org_id`, `organization`), or a link ending in them. Nothing needs recompiling, and on these routes:

- a resource a search, a mention, a fixture or a session seed stored without showing its owner is no longer served by name. The read is generated, and the response cache repeats it. Give such a fixture a member that shows its owner;
- only a member named for the parameter, `name`, `login`, `slug` or `key`, or a path holding the owner too, identifies a resource by name. Any other string does not;
- `unknown_ids: 404` does not answer a value some owner's resource goes by. A value none goes by is still a 404;
- a write acts unless the resource is known to be another's, by a record, by what it shows or by a link elsewhere. Then a delete leaves it and an update does not change it. Another owner's create can still replace a resource with the same id.

The collection record lives in the node's memory, like path aliases, and ends when the resource changes without it. On another node or after a restart a read relies on what the resource shows, except that a number showing nothing is served, and a resource written through the route under its name, with an `id` of its own, reads back for any owner. A string id no name member holds, such as `txn_1` below `/customers/{customer}`, reads back as before unless it was listed under another collection or a member tied to the parameter names another owner.

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
