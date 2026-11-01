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

### Write memory

No migration or configuration. With an LLM and `consistency: on`, on HTTP routes ([what it does](concepts.md#sessions-and-state)):

- **Reads can make an extra LLM call** after a write that answered little, where they answered `state` before. One call catches up however many writes are waiting; a failing catch-up tries twice, each up to two calls, then stops until a later write. `consistency: off` and `--offline` make none.
- **A `GET` can publish an upsert**: a catch-up that changes the stored copy replaces it, so `on_state_change` triggers, subscriptions and change feeds can fire on a read, and the resource moves to the front of its type's list order, as any write does. One that changes nothing publishes nothing and keeps its place.
- **A remembered action publishes a change of its target at write time**, with the copy as it stood, where it published none, and moves it to the front of the list order. A create or a body-less action that tells a parent does **not** publish a change of that parent or move it, as nothing it holds changed: another node's read looks in the shared store before it serves. The exception is a read a node has cached that the stored copy does not answer: a query view of a shorter copy, or a non-detail view such as `/posts/{id}/summary`. It stays until something else invalidates it, so another node can answer it stale, as on earlier releases. An effect that changes a parent publishes as it always did.
- **A 204 update merges** the members its read's schema declares into the stored copy, instead of replacing the copy with the request. An update naming its target by a field its reads go by (a key) now finds it, instead of storing a second resource under that key. The merge leaves identity and path fields and `readOnly` members alone, which is intended: `PUT /pets/p1 {"id":"p2"}` used to make `/pets/p1` answer `id: "p2"`, and now it stays `p1`.
- **Recorded prompts**: a snapshot or recording taken before this release may not replay after a write, as the read now generates with an `<earlier_requests>` block. Reads with no remembered request send the same prompt as before, to the byte.
- **Rolling upgrades**: documents of the reserved type `__mockingbird.earlier_requests` are kept in the session's state. A node of an older release exports and counts them as resources, and a fixture exported through one may hold them; newer nodes drop them when seeding. Export sessions from upgraded nodes.
- **Rust API**: `GenerationContext` gains a public field, `earlier_requests`, which breaks code constructing it in full; add `..Default::default()`. `Engine::scrub` is public. `GenerationContext` also gains `earlier_own_body`. `RequestRecorder` gains a defaulted `redact`, which an implementor keeping request bodies should provide, or only the built-in credential and PII scrubbing applies.
- **Credential names**: one predicate, `mb_mcm::credentials::is_credential_name`, now decides for request headers, import redaction and the memory. It adds `session`, `credential`, `passwd`, `privatekey`, `sshkey`, `signingkey`, `cvv`, `bearer`, whole-word `auth` and `pin` to what headers treat as credentials, so a header such as `x-session-id` drops from recordings (prompts never carried request headers). `X-Mockingbird-*` overrides are exempt. A contract declaring a header does not exempt it, since contracts routinely declare `Authorization`.
- **A 204 update with a valid body plus an undeclared member costs one extra LLM call** on the next read, as intended: the member cannot go in the stored copy, so the read is told of it. A remainder holding only redacted values, such as `password`, is not remembered and costs no call.
- **The path says whose a resource is**: a 204 update's member contradicting the owner in its path (`PUT /orgs/octo/teams/core {"organization":"acme"}`) is neither applied nor remembered.

### Action routes act on their parent resource

A `POST <static>/{param}/<verb>` the contract shows acting on its parent ([how that is read](import.md#how-a-post-under-a-resource-is-read)), such as `POST /v1/payment_intents/{intent}/confirm`, used to compile as a create on a resource type named after `<verb>`. It now acts on `<static>`: an update when it answers what `GET <static>/{param}` answers, otherwise a command that stores nothing. Operation ids and routes stay the same. These change:

- its operation's `kind`, `resource_type` and `identity` in the compiled bundle, while `source_digest`, which hashes the source, does not;
- its webhook event type, from `confirm.created` to `payment_intent.updated`, or for a command that answers a record of its own, from `cancel.created` to `booking.updated`;
- what state it writes: resources stored under the old type (`confirm`) in a session or a session snapshot are no longer read by anything, and `on_state_change` triggers on that type stop firing.

A bundle takes this on recompile, and stored bundles are not recompiled on their own. For a mock managed through the API, upload the contract again. For a mock from the config file, restart: with an in-memory control store that is enough, but with a persistent one a mock keeps its active version while the contract's digest matches it, so also change a byte of the contract, such as a comment.

### Action routes come from the contract's structure

`POST <static>/{param}/<static>` used to be an action only if its last segment did not end in `s`, it had no `GET` on its path and no route below it. The spelling no longer counts, and [the contract decides](import.md#how-a-post-under-a-resource-is-read). On recompile:

- A `POST` whose successes declare no body acts on its parent and stores nothing, even with a `GET` beside it or a longer static tail: Jira's `POST /issue/{issueIdOrKey}/transitions` and Auth0's `POST /users/{id}/roles`. It sends no webhook unless the contract gives it a payload.
- A bodyless `POST` whose last segment is a read verb (`POST /jobs/{id}/sync` answering `204`) was a read and is now an action on its parent.
- A plural tail that answers what the parent's `GET` answers (`POST /orders/{id}/items` answering the order, with nothing below it) was a create of `items` and is now an update of the order.
- An update on an action route whose LLM answer carries an identity other than its target's is [a new record of the parent's type](import.md#an-update-that-answers-another-record), stored under its own id, or a fresh one if a record holds that id, with the target untouched, and sends `<type>.created`. If no fresh id can be drawn, the answer is served and not stored. The id used to be overwritten with the target's. A faker answer, an answer with no identity, or a call with no stored target updates the target as before. This depends on the model: for a reply with a stored parent, a new id came back 8 of 8 times with Luna and the default model and at most 2 of 8 with Lightning, so a reply can still overwrite its parent. Where every call makes a new record, set `x-mockingbird-resource: {action: false, type: <the parent's type>}`.
- An action answering `{type: object}` beside a parent's `GET` answering `{type: object}` moves from an update to `Effect::Other`: it stores nothing, since two open schemas do not say the answer is the parent.
- A `POST` answering `200`, whose `GET` beside it reads something else or that has no `GET` and nothing else reading its answer, acts on its parent whatever its spelling: `POST /v1/customers/{id}/sources`, `/{id}/process`, `/{id}/address`.
- A singular tail declaring a `201`, which acted on its parent unless it answered the parent's record, is now a create of its own, even with no identity (when it stores nothing). It loses its parent as a target: with no identity path parameter it has no stored target, and its prompt shows the parent only if the path parameter is named for a type (`{booking_id}`, not `{id}`).
- A `POST` whose answer another resource's `GET` serves by path is a create under that route's type: PayPal's `POST /captures/{capture_id}/refund` is a create of `refunds`, so `GET /refunds/{id}` serves it. It was a command on `captures` that stored nothing, and it now has no capture as its target.
- A `POST` declaring a `201` beside a `200` is judged by the `201`.

For each operation that changes, these follow:

- its `kind`, `resource_type` and `identity` in the compiled bundle. Routes, operation ids and `source_digest` stay the same;
- its webhook event type, from `roles.created` to nothing at all for a bodyless action, from `capture.updated` to `refund.created`, and to `<type>.created` for an action's new record. `Bundle::webhook_for` also finds a contract's payload by event type, so a webhook the contract names after the old event type (`capture.updated`) stops matching this operation: the event goes out as the default envelope unless a callback of the operation or a webhook named after its id applies;
- what state it writes: resources stored under the old type in a session or snapshot are no longer read by these operations, and `on_state_change` triggers on that type stop firing for them.

Set [`x-mockingbird-resource`](import.md#correcting-it-x-mockingbird-resource) on an operation to keep what it was. It is per operation. Contracts without the extension behave as inference says.

Stored bundles take this the way they take action routes, with the same steps: upload the contract again, or restart a mock from the config file, changing a byte of the contract if the control store is persistent.

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
