# Audit: mb-admin + mb-auth (verified)

Adversarial pass over 54 findings: 51 kept (some corrected), 3 dropped (STUB-12, TESTS-6, TESTS-7). All verification was read-only (grep/sed); no cargo or npm run.

## Perf

### MB-ADMIN-PERF-1
- file: crates/mb-auth/src/apikey.rs:188
- dimension: perf | guideline: PERF:per-request-store-roundtrip | severity: medium | semver: internal-only | effort: M | classification: n/a
- evidence: `ApiKeys::verify` always calls `store.get_api_key(&key_id)`. `StoreDataPlaneKeys::verify` (crates/mockingbird/src/dp_keys.rs:27) runs on every data-plane call to a mock in api_key mode, and it holds its own `ApiKeys` with no cache. The `touch_api_key` write (apikey.rs:210-215) is not coalesced, so concurrent requests that all see a stale `last_used_at` all write it.
- fix: Add a short-TTL cache (about 5 s) inside ApiKeys, keyed by key_id and holding the row. Keep the constant-time hash comparison and the revoked, expiry and scope checks on every call. Coalesce the touch with an in-memory last-touched timestamp per key.
- risk: A revoked key stays valid for up to the TTL on nodes that did not process the revoke, and longer in split mode where the data plane runs separately. Document this. Never cache by raw token.

### MB-ADMIN-PERF-2
- file: crates/mb-admin/src/mocks.rs:452 (helper `view` at :434)
- dimension: perf | guideline: PERF:n-plus-1 | severity: medium | semver: additive | effort: M | classification: n/a
- evidence: `list` awaits `view()` for each mock in turn. `view()` calls `active_contract`, which selects VERSION_SELECT (postgres.rs:46: source bytea plus bundle::text) and parses the bundle, only to read `.version`.
- fix: Add a defaulted `ControlStore::active_versions(mock_ids) -> HashMap<Uuid, i64>` that runs one query, with a default implementation that loops over `active_contract`. Use it in list, get_one and patch.
- risk: Low. The trait method must have a default, because mockingbird-store is published and the trait can be implemented outside the crate.

### MB-ADMIN-PERF-3
- file: crates/mb-admin/src/contracts.rs:439
- dimension: perf | guideline: PERF:n-plus-1 | severity: medium | semver: additive | effort: L | classification: n/a
- evidence: `ensure_grpc_services_free` iterates `list_all_mocks()` and awaits a full `active_contract` for every other mock. It runs on upload-with-activate (contracts.rs:385), activate and approve whenever the bundle has gRPC services. The cost is O(total mocks) queries plus the full contract bytes of every mock.
- fix: Keep an index of the gRPC services each active version serves (an appended migration) and look them up with one query. A cheaper interim fix is a defaulted store method that returns (mock_id, bundle) for active versions without the source.
- risk: The index must be updated in the activation transaction, or the conflict check races.

### MB-ADMIN-PERF-4
- file: crates/mb-admin/src/contracts.rs:476
- dimension: perf | guideline: PERF:overfetch | severity: medium | semver: additive | effort: M | classification: n/a
- evidence: `contracts::list` calls `list_contract_versions` (postgres.rs:804, which selects source and bundle for every version) only to build summaries. imports/worker.rs:255 does the same to find the newest Draft or Active version. ControlStore has no delete or prune for contract versions, so this cost grows with every upload and import.
- fix: Add a defaulted summary query that returns metadata and operation/channel counts (jsonb_array_length / json_array_length), plus `latest_contract_version(mock_id, statuses)` with LIMIT 1 for the worker.
- risk: The summary must stay in step with ContractVersionSummary on both backends. Pruning is tracked separately in STUB-6.

### MB-ADMIN-PERF-5
- file: crates/mb-admin/src/live.rs:209
- dimension: perf | guideline: PERF:clone-hot-path | severity: medium | semver: breaking-needs-0.2 | effort: M | classification: n/a
- evidence: `LiveSubscribe` is `Arc<dyn Fn() -> broadcast::Receiver<RequestRecord>>` (live.rs:41, re-exported at lib.rs:57, a pub field of AdminState). `broadcast::recv` clones the whole record, including the `request` and `response` JSON Values, for every open stream before the workspace and mock filter runs.
- fix: In 0.2, broadcast `Arc<RequestRecord>` and convert only the records that match, or fan out per (workspace, mock).
- risk: Changes a public type alias and the recorder in the mockingbird crate.

### MB-ADMIN-PERF-6
- file: crates/mb-admin/src/requests.rs:115
- dimension: perf | guideline: PERF:overfetch | severity: low | semver: additive | effort: M | classification: n/a
- evidence: `fields=summary` still loads the full bodies, and `json_len` (requests.rs:107) re-serializes each one only to measure it. A page holds up to 500 rows, and each stored body is capped at about 64 KiB.
- fix: Compute the sizes in the store (`octet_length(request::text)`) or store them at insert time, behind a defaulted summary listing method.
- risk: The sizes must match the current definition (JSON length of the redacted value).

### MB-ADMIN-PERF-7
- file: crates/mb-admin/src/drafts.rs:302
- dimension: perf | guideline: PERF:blocking-on-async | severity: low | semver: internal-only | effort: S | classification: n/a
- evidence: `draft_view` runs `serde_json::from_slice(&v.source)` (up to MAX_CONTRACT_BYTES) and `diff_bundles` on the runtime thread, and fetches a second full contract through `active_contract` (drafts.rs:308). `put_draft` already moves compilation to spawn_blocking (drafts.rs:399).
- fix: Move the parse and the diff into spawn_blocking, and fetch only the active bundle and version.
- risk: Low.

### MB-ADMIN-PERF-8
- file: crates/mb-admin/src/contracts.rs:383
- dimension: perf | guideline: PERF:overfetch | severity: low | semver: additive | effort: S | classification: n/a
- evidence: `active_contract(m.id).await?.is_none()` loads the full source only to get a boolean. channels.rs:319, scenarios.rs:313 and drafts.rs:308 also throw the source away. This is related to PERF-2, PERF-3 and PERF-7 and can share their store methods.
- fix: Add defaulted `has_active_contract` and `active_bundle` store methods that leave out the source column.
- risk: Low.

### MB-ADMIN-PERF-9
- file: crates/mb-admin/src/scenarios.rs:301
- dimension: perf | guideline: PERF:quadratic-scan | severity: low | semver: internal-only | effort: S | classification: n/a
- evidence: Each `operation_instructions` key is checked with `body.operations.iter().any(..)`, which is O(k*m). Then every name goes through `Bundle::operation`, a linear find (mb-mcm lib.rs:357-358). The only bound is the request body size limit.
- fix: Build HashSets once, and cap both lists (for example at 1,000) with a 422.
- risk: A cap could reject an existing large scenario, so choose a generous limit.

### MB-ADMIN-PERF-10
- file: crates/mb-admin/src/webhook.rs:64
- dimension: perf | guideline: PERF:head-of-line | severity: low | semver: internal-only | effort: M | classification: n/a
- evidence: One task awaits `deliver` for each event in turn. With 4 attempts at a 5 s timeout plus 1+2+4 s of backoff, one event can take about 27 s, so the 1,024-slot queue fills and later events are dropped. The body Vec is cloned for each attempt (line 123).
- fix: Deliver with bounded concurrency, or move retries to a delay queue. Use `Bytes` for the body.
- risk: Delivery order would change. The module doc (webhook.rs:3-4) currently describes delivery as sequential, and TESTS-4 relies on that.

### MB-ADMIN-PERF-11
- file: crates/mb-admin/src/sessions.rs:112
- dimension: perf | guideline: PERF:unbounded-response | severity: low | semver: additive | effort: S | classification: n/a
- evidence: `st.state.sessions(ws, mock)` returns every live session (mb-state lib.rs:137) and the handler serializes all of them. Data-plane clients choose session ids freely.
- fix: Add optional `limit`/cursor parameters. With no parameter, behaviour stays the same.
- risk: Additive only if the parameter is optional.

### MB-ADMIN-PERF-12
- file: crates/mb-auth/src/session.rs:65
- dimension: perf | guideline: PERF:per-request-store-roundtrip | severity: low | semver: internal-only | effort: M | classification: n/a
- evidence: `lookup` runs `get_admin_session` plus a JSON decode on every request. `console_gate` wraps the console's static fallback (mockingbird/src/config.rs:822), so every asset pays this cost. `WsAccess` adds `get_workspace` (access.rs:116).
- fix: Add a 1-5 s cache keyed by id_hash that `destroy` evicts, or exempt fingerprinted immutable assets from the gate.
- risk: A logout on another node takes effect up to the TTL later.

## API and style

### MB-ADMIN-STYLE-1
- file: crates/mb-auth/src/oidc.rs:73 (also apikey.rs:47)
- dimension: api | guideline: C-DEBUG / repo:secrets-as-references | severity: medium (was high: latent, no call site prints either type) | semver: none | effort: S | classification: n/a
- evidence: `OidcSettings` derives Debug, and its `client_secret` is the resolved secret value. `GeneratedKey` derives Debug, and its `token` is the full credential. Both are re-exported from the crate root (lib.rs:20-30).
- fix: Write manual Debug impls that redact `client_secret` and `token`.
- risk: Very low.

### MB-ADMIN-STYLE-2
- file: crates/mb-admin/src/channels.rs:84 (and problem.rs:24)
- dimension: api | guideline: C-GOOD-ERR | severity: medium | semver: additive | effort: S | classification: n/a
- evidence: `ChannelControlError` (in pub mod channels; the error type of the public `ChannelControl` trait) and `Problem` (re-exported at lib.rs:61; the Err type of ApiResult) implement neither Display nor std::error::Error. Correction: DraftError is in the private `drafts` module and is not re-exported, so it is not public API and is out of scope.
- fix: Add Display impls (ChannelControlError delegates to its existing private `detail()` at channels.rs:104) and `std::error::Error` impls (Partial's `source()` returns the inner error). mb-admin does not depend on thiserror today, so either write the impls by hand or add the dependency.
- risk: Additive.

### MB-ADMIN-STYLE-3
- file: crates/mb-auth/src/http.rs:17
- dimension: api | guideline: C-STRUCT-PRIVATE / Google:non-exhaustive-enums | severity: low | semver: breaking-needs-0.2 | effort: S | classification: n/a
- evidence: `pub struct HttpError(pub String)`, and the in-crate test reads `err.0` (http.rs:93). `AuthError` (error.rs:4) has no type-level doc, its `Store` variant is undocumented, and it is not `#[non_exhaustive]`.
- fix: In 0.1.x, add `HttpError::message()` and the docs. In 0.2, make the field private and mark AuthError non_exhaustive. mb-admin problem.rs:179 would then need a `_ =>` arm.
- risk: The 0.2 part is breaking.

### MB-ADMIN-STYLE-4
- file: crates/mb-auth/src/role.rs:23
- dimension: api | guideline: Google:std-traits / C-COMMON-TRAITS | severity: low | semver: additive | effort: S | classification: n/a
- evidence: `Role::parse` and `as_str` re-implement FromStr and Display. KeyScope has `as_str` but no Display. PrincipalKind has neither, which is why audit.rs `actor_kind` goes through serde_json::to_value.
- fix: Add FromStr for Role (with a ParseRoleError), Display for Role, KeyScope and PrincipalKind, and `PrincipalKind::as_str`.
- risk: Additive.

### MB-ADMIN-STYLE-5
- file: crates/mb-auth/src/apikey.rs:111
- dimension: api | guideline: C-DEBUG | severity: low | semver: additive | effort: S | classification: n/a
- evidence: Confirmed with no Debug impl: ApiKeys, Authenticator, JwtVerifier, OidcClient, MockOidcProvider, AdminState, Auditor, WebAuth, Imports, ImportWorkers, OfflineLlm and problem::RequireJson (reachable through pub mod problem).
- fix: Add manual Debug impls with `finish_non_exhaustive()`, and derive Debug on the unit structs.
- risk: Additive. The OidcClient impl must not print `id_keys`, which holds the client secret.

### MB-ADMIN-STYLE-6
- file: crates/mb-auth/src/oidc.rs:192
- dimension: api | guideline: C-FAILURE / Google:panic-docs | severity: low | semver: none | effort: M | classification: n/a
- evidence: There are no `# Errors`, `# Panics` or `# Examples` sections anywhere in either crate's src (grep count 0), although these APIs are fallible and public: discover, begin_login, complete_login, ApiKeys::create and verify, and the SessionManager methods. The testing helpers panic by design.
- fix: Add `# Errors` sections naming the AuthError variants, and `# Panics` sections on MockOidcProvider.
- risk: Docs only.

### MB-ADMIN-STYLE-7
- file: crates/mb-auth/src/principal.rs:47
- dimension: api | guideline: C-CRATE-DOC / C-EXAMPLE | severity: low | semver: none | effort: M | classification: n/a
- evidence: Undocumented: `Principal::can`, `PrincipalKind`, the Principal fields, `Role::as_str` and `parse`, `Permission::min_role`, `SessionManager::new`, `OidcClient`, and the handlers in pub mod channels. Neither crate has missing_docs enabled or a crate-level example.
- fix: Add `#![warn(missing_docs)]` to mb-auth and fill the gaps. Document the pub channels handlers rather than narrowing their visibility.
- risk: Docs only.

### MB-ADMIN-STYLE-8
- file: crates/mb-admin/src/mocks.rs:26
- dimension: lint | guideline: lint:allow-dead-code | severity: low | semver: none | effort: S | classification: n/a
- evidence: 12 `#[allow(dead_code)]` attributes on ToSchema mirror types: 9 in mocks.rs, plus apikeys.rs:22, workspaces.rs:23 and auth_routes.rs:80.
- fix: Replace each with `#[expect(dead_code, reason = "OpenAPI schema mirror")]`.
- risk: Under `--all-targets --all-features`, `expect` fails if the lint does not fire in some cfg. Check each one under the CI command.

### MB-ADMIN-STYLE-9
- file: crates/mb-admin/src/access.rs:132
- dimension: lint | guideline: lint:clippy.toml allow-unwrap-in-tests | severity: low | semver: none | effort: S | classification: n/a
- evidence: There is no clippy.toml. 5 in-src test modules carry `allow(clippy::unwrap_used)`: access.rs:132, contracts.rs:695, problem.rs:298, auth_routes.rs:424 and mb-auth http.rs:81. All 18 integration-test files carry inner allows.
- fix: Add a clippy.toml with `allow-unwrap-in-tests = true` and remove the in-src allows. Keep the integration-file allows, because helper fns outside #[test] are not covered.
- risk: Low.

### MB-ADMIN-STYLE-10
- file: Cargo.toml:127
- dimension: lint | guideline: repo:no-unwrap-expect | severity: medium | semver: none | effort: S | classification: n/a
- evidence: `[workspace.lints.clippy]` contains only `unwrap_used = "deny"`, while AGENTS.md also forbids expect(). The only non-test `.expect(` in this group is in the feature-gated testing.rs, but other crates also have files with expect: mb-protocols graphql/mod.rs, mb-state bus.rs, and mockingbird maintenance.rs, ha.rs and mcp.rs.
- fix: Add `expect_used = "deny"` once the other crates are audited (or start at warn). Add `#![expect(clippy::expect_used, reason = ...)]` to testing.rs and allow-expect-in-tests to clippy.toml. Treat `panic = "deny"` as optional and separate.
- risk: Workspace-wide, so it can fail -D warnings in other crates.

### MB-ADMIN-STYLE-11
- file: crates/mb-auth/src/session.rs:32
- dimension: api | guideline: Google:enum-over-bool | severity: low | semver: additive | effort: S | classification: n/a
- evidence: `SessionManager::new(store, ttl, secure: bool)` and `WebAuth { secure_cookies: bool }`.
- fix: Add a `CookieSecurity` enum and a `with_cookie_security` constructor, keeping `new`.
- risk: Additive.

### MB-ADMIN-STYLE-12
- file: crates/mb-auth/src/apikey.rs:221
- dimension: api | guideline: C-CONV-TRAITS | severity: low | semver: additive | effort: S | classification: n/a
- evidence: `ApiKeys::principal(key: &ApiKey) -> Principal` takes no self and is a plain conversion. `create` returns the tuple `(ApiKey, String)`.
- fix: Add `impl From<&ApiKey> for Principal` and delegate to it. Defer the named return struct to 0.2.
- risk: Additive.

### MB-ADMIN-STYLE-13
- file: crates/mb-admin/src/audit.rs:62
- dimension: style | guideline: lint:too_many_arguments | severity: low | semver: internal-only | effort: S | classification: n/a
- evidence: `pub(crate) record_by` takes 8 inputs and carries `#[allow(clippy::too_many_arguments)]`. Correction: it has 4 call sites, not 5 (audit.rs:113, workspaces.rs:135, auth_routes.rs:225 and :333).
- fix: Add an `AuditEntry` parameter struct and drop the allow.
- risk: Internal only.

### MB-ADMIN-STYLE-14
- file: crates/mb-admin/src/imports/mod.rs:616
- dimension: style | guideline: Google:no-api-in-nonleaf | severity: low | semver: none | effort: M | classification: n/a
- evidence: imports/mod.rs (808 lines) declares `pub mod llm; pub mod worker;` and also holds the settings, schemas, views and the create, list, get_one and cancel handlers (616, 732, 757 and 783).
- fix: Split into handlers.rs and types.rs, and `pub use` them so every `mb_admin::imports::*` path still resolves.
- risk: A mechanical move.

### MB-ADMIN-STYLE-15
- file: crates/mb-auth/src/authenticator.rs:38
- dimension: api | guideline: C-STRUCT-PRIVATE / Google:non-exhaustive-structs | severity: low | semver: breaking-needs-0.2 | effort: M | classification: n/a
- evidence: Authenticator has 3 pub fields, AdminState (lib.rs:84-114) has 13, and WebAuth has pub fields too. None of these structs is non_exhaustive, so adding a field breaks downstream struct literals. AdminState already has `new()` (lib.rs:122).
- fix: In 0.1.x, add `with_*` setters and `Authenticator::new`. In 0.2, make the fields private or mark the structs non_exhaustive.
- risk: The 0.2 part is breaking.

### MB-ADMIN-STYLE-16
- file: crates/mb-admin/src/lib.rs:7
- dimension: style | guideline: Style:comment-width | severity: low | semver: none | effort: S | classification: n/a
- evidence: Narrowed: the repo has no rustfmt.toml, so max_width is the default 100. The only comment line in mb-admin/src over 100 columns is lib.rs:7 (117 columns), where a new sentence was run onto the line. The other cited lines are 81-92 columns, which is within the code width and not a violation.
- fix: Break lib.rs:7 after "`/auth`." so the next sentence starts its own line.
- risk: None.

## Stubs and gaps

### MB-ADMIN-STUB-1
- file: crates/mb-auth/src/oidc.rs:182
- dimension: stub | guideline: n/a | severity: high | semver: internal-only | effort: M | classification: finish
- evidence: `pending: Mutex<HashMap<String, Pending>>` is process memory, and `complete_login` (oidc.rs:303) only looks there. Correction: the Helm default `mode: allInOne` runs 1 replica. In `mode: split`, `control.replicas` defaults to 2 (values.yaml:105), and service-admin.yaml has no sessionAffinity anywhere in deploy/, so about half of callbacks land on the other pod and fail with "unknown login state".
- fix: Persist pending logins in an appended `oidc_pending` table, or seal them in the `mb_login` cookie with AEAD. As a stopgap, set `sessionAffinity: ClientIP` for split mode when replicas > 1, and document it.
- risk: Security-sensitive. Keep the state single-use with the 600 s TTL and the MAX_PENDING bound. A new table is a Postgres migration and needs review.

### MB-ADMIN-STUB-2
- file: crates/mb-admin/src/auth_routes.rs:320
- dimension: stub | guideline: n/a | severity: medium | semver: additive | effort: M | classification: finish
- evidence: docs/console.md:24 tells operators to register a post-logout redirect URI, but nothing in crates/ or docs/ uses `end_session_endpoint`. logout returns 204, and the console (Layout.tsx:174-180) goes straight to /auth/login, where the IdP session signs the user back in.
- fix: Read end_session_endpoint from discovery and return the IdP logout URL with post_logout_redirect_uri. Without the endpoint, keep today's behaviour and fix the doc line.
- risk: A wire change on a 204 endpoint. Logout must still succeed locally when the IdP is down.

### MB-ADMIN-STUB-3
- file: crates/mb-admin/src/lib.rs:156
- dimension: stub | guideline: n/a | severity: medium | semver: internal-only | effort: S | classification: ask-user
- evidence: `tenancy(ws)` hardcodes DEFAULT_TENANT, and it is used by imports/mod.rs:682, drafts.rs:549 and imports/worker.rs:321. The data plane calls `tenants.resolve` (mb-protocols call.rs:470). The DEFAULT_TENANT doc at lib.rs:151 presents this as deliberate, but drafts.rs:548 says "Admitted and reported like a data-plane call", so the code contradicts itself and a custom mb-ext TenantResolver is skipped.
- fix: If the user confirms: resolve the tenant through `st.ext.tenants.resolve(&call_info)` and store the resolved tenant on the import job. Otherwise, document that admin LLM work is always billed to `default`.
- risk: A resolver that requires data-plane headers would start refusing imports.

### MB-ADMIN-STUB-4
- file: crates/mb-admin/src/workspaces.rs:242
- dimension: stub | guideline: n/a | severity: medium | semver: none | effort: S | classification: finish
- evidence: With force=true, the handler resets state and calls `delete_workspace(ws, true)`. The SQLite implementation (sqlite.rs:553+) deletes mocks and profiles without checking their source, while the per-resource routes refuse file-managed ones (`ensure_api_managed`). Reconcile then recreates the file-managed resources on the next reload.
- fix: Return 409 managed_by_file, naming the resources, when any mock or profile has ResourceSource::File.
- risk: Low. The change only adds a refusal.

### MB-ADMIN-STUB-5
- file: crates/mb-admin/src/scenarios.rs:287
- dimension: stub | guideline: n/a | severity: medium | semver: none | effort: M | classification: finish
- evidence: `set` is only checked with `is_object()`. docs/improvements.md:71 already records that an invalid `set` is dropped at request time with only a WARN.
- fix: Validate the keys present in `set` against the response schemas the scenario can select, return a 422 naming the pointer, and share the check with config validation.
- risk: Some existing scenarios would start returning 422. Validate only the keys present, not required-ness.

### MB-ADMIN-STUB-6
- file: crates/mb-admin/src/lib.rs:219
- dimension: stub | guideline: n/a | severity: medium | semver: additive | effort: M | classification: finish
- evidence: The contract routes (lib.rs:208-243) have no DELETE, and ControlStore has prune methods only for token usage, audit and import jobs, none for contract versions. Rejected imported drafts (each up to 20 MiB) can never be removed. This also feeds PERF-4.
- fix: Add `DELETE …/contracts/{version}` (Edit permission; 409 for the active version or a file-managed mock; audit event `contract.delete`), a defaulted `ControlStore::delete_contract_version`, and a console Discard control. Then regenerate the console client.
- risk: Import jobs reference draft versions. The trait method needs a default.

### MB-ADMIN-STUB-7
- file: crates/mb-auth/src/session.rs:82
- dimension: stub | guideline: n/a | severity: low | semver: additive | effort: M | classification: ask-user
- evidence: `last_seen` is written at most once a minute (session.rs:81-83) but never read. Roles are snapshotted at login, the TTL is absolute (12h default, 30d maximum), and there is no API to list or revoke sessions.
- fix: A product decision. Either add an idle timeout plus sysadmin session revocation, or drop the touch. Either way, document that roles are a snapshot taken at login.
- risk: A new config key needs docs/configuration.md and mockingbird.full.yaml updates.

### MB-ADMIN-STUB-8
- file: crates/mb-auth/src/jwt.rs:18
- dimension: stub | guideline: n/a | severity: low | semver: none | effort: S | classification: keep-refusal
- evidence: ALLOWED_ALGS is RS256, ES256 and EdDSA only. A `kid` is required (jwt.rs:149). The userinfo roles fallback runs only on console login (oidc.rs:348), not for bearer tokens. docs/console.md:10 says "any compliant provider works" and documents none of these limits.
- fix: Keep the hardening and document the limits. Optionally log at debug when a token maps to zero roles.
- risk: None.

### MB-ADMIN-STUB-9
- file: crates/mb-auth/src/mapping.rs:69
- dimension: stub | guideline: n/a | severity: low | semver: additive | effort: S | classification: finish
- evidence: Groups with an unknown role, a missing segment or an invalid workspace are skipped with `continue` (mapping.rs:55-75) and nothing is logged.
- fix: Log the ignored prefixed groups once per login at info, and optionally return them as `ignored_groups` in MeView.
- risk: Keep group names out of audit snapshots.

### MB-ADMIN-STUB-10
- file: crates/mb-admin/src/mocks.rs:381
- dimension: stub | guideline: n/a | severity: low | semver: none | effort: M | classification: ask-user
- evidence: None of `webhooks`, `path_rewrite`, `unknown_ids`, `default_scenario`, `consistency` or `deliveries` appears in mb-console/web/src outside schema.d.ts (`hosts` appears only in the test harness). The console never calls workspace PATCH or DELETE, although docs/console.md lists those as owner capabilities.
- fix: Decide the console's scope: either add the UI, or document these as API/CLI/MCP only.
- risk: UI only.

### MB-ADMIN-STUB-11
- file: crates/mb-auth/src/http.rs:21
- dimension: stub | guideline: n/a | severity: low | semver: none | effort: S | classification: finish
- evidence: `http_client()` falls back to `reqwest::Client::new()`, which follows redirects and has no timeout, against the function's doc. Correction: tests do call it (mb-auth tests/oidc.rs, mb-admin tests/auth_flow.rs), but non-test code does not.
- fix: Build the fallback with `redirect(Policy::none())` and the timeout, or deprecate the function in favour of `http_client_with`.
- risk: Very low.

### MB-ADMIN-STUB-13
- file: docs/admin-api.md:30
- dimension: stub | guideline: n/a | severity: low | semver: none | effort: S | classification: finish
- evidence: The content-types bullet leaves out the `application/vnd.aai.asyncapi` media types (and their `+json`/`+yaml` forms) that CONTRACT_MEDIA_TYPES accepts (contracts.rs:37-39). The resource table (admin-api.md:9-18) leaves out workspaces, api-keys, audit, /me, requests/{id}, requests/stream and webhooks/deliveries.
- fix: Add the missing media types and rows, and optionally a test that checks the docs against CONTRACT_MEDIA_TYPES.
- risk: None.

## Tests and build

### MB-ADMIN-TESTS-1
- file: crates/mb-auth/tests/oidc.rs:395
- dimension: tests | guideline: PERF:fixed-sleep-in-test | severity: high | semver: none | effort: S | classification: n/a
- evidence: The JWKS mock is mounted with `set_delay(3s)` (line 385), then the test sleeps 300 ms (line 395), and `slow.await` (line 409) waits out the full 3 s.
- fix: Cut the delay to about 1 s, or abort `slow` and check the Upstream outcome in a separate test that uses a 500 with no delay. Replace the 300 ms sleep with a poll on `jwks_requests`, but only after confirming that wiremock records a request before its delayed response is sent; otherwise keep a short sleep.
- risk: Low. Keep the delay well above the 1 s budget for the cached verifications.

### MB-ADMIN-TESTS-2
- file: crates/mb-auth/tests/core.rs:501
- dimension: tests | guideline: PERF:fixed-sleep-in-test | severity: medium | semver: none | effort: S | classification: n/a
- evidence: A 1 s TTL followed by `sleep(1200ms)`. `lookup` compares against `Utc::now()`, so pausing tokio's clock does not help.
- fix: Insert an already-expired AdminSession directly through `ControlStore::create_admin_session`, then assert that lookup returns None and the row is deleted. Correction: no existing test checks the ttl-to-expires_at computation, so add a small one that reads `expires_at` back after `create`.
- risk: Low.

### MB-ADMIN-TESTS-3
- file: crates/mb-auth/src/testing.rs:31
- dimension: tests | guideline: PERF:rsa-keygen-in-tests | severity: medium | semver: internal-only | effort: M | classification: n/a
- evidence: A fresh 2048-bit RSA key is generated on every `start()` (line 86) and `rotate_key()` (line 150). That is 19 starts in oidc.rs, a harness start in each of the 21 auth_flow tests, and 6 rotations. The workspace Cargo.toml already sets rsa to opt-level 3 because keygen is slow.
- fix: Keep a process-wide pool (a OnceLock) of 4 or more keys, or check in test-only PEMs, and give each provider unique kids.
- risk: Low. Internal to the testing feature.

### MB-ADMIN-TESTS-4
- file: crates/mb-admin/tests/audit.rs:895
- dimension: tests | guideline: PERF:fixed-sleep-in-test | severity: low (was medium: saves about 0.3 s) | semver: none | effort: S | classification: n/a
- evidence: The test sleeps 300 ms to show there is no fifth attempt, while the backoffs are 10, 20 and 40 ms.
- fix: Send event 2, wait for the 5th request, and assert that exactly 4 requests carry event id 1. Delivery is sequential, so event 2 can only start after event 1's retries are done.
- risk: Relies on sequential delivery (see PERF-10).

### MB-ADMIN-TESTS-5
- file: crates/mb-admin/tests/audit.rs:983
- dimension: tests | guideline: PERF:fixed-sleep-in-test | severity: low | semver: none | effort: S | classification: n/a
- evidence: The test sleeps 200 ms to check that the retention task with u32::MAX did not panic. The first tick fires immediately and does no I/O when the cutoff is None.
- fix: Yield a few times (`yield_now`) on the current_thread runtime, then assert `!is_finished()`.
- risk: Low.

### MB-ADMIN-TESTS-8
- file: crates/mb-admin/tests/audit.rs:930
- dimension: tests | guideline: tests:assert-behaviour | severity: low (was medium: the mpsc channel is bounded by construction) | semver: none | effort: S | classification: n/a
- evidence: The test sends 2000 events into a queue of 1024 and asserts only that it took under 1 s. It never checks the `mb_audit_webhook_dropped_total` counter.
- fix: Wrap the loop in `metrics::with_local_recorder` and assert the dropped count is 975 or 976.
- risk: Low.

### MB-ADMIN-TESTS-9
- file: crates/mb-admin/tests/audit.rs:725
- dimension: tests | guideline: tests:weak-assertion | severity: low | semver: none | effort: S | classification: n/a
- evidence: `ids.dedup()` removes only adjacent duplicates, so the assertion proves uniqueness only if the ids are already sorted.
- fix: `assert!(ids.windows(2).all(|w| w[0] > w[1]))` (newest first).
- risk: None.

### MB-ADMIN-TESTS-10
- file: crates/mb-admin/tests/api.rs:1
- dimension: tests | guideline: tests:binary-consolidation | severity: medium | semver: none | effort: M | classification: n/a
- evidence: 16 integration-test files, so 16 linked binaries. No [[test]] entries in mb-admin's Cargo.toml. No set_var, `mod common` or global recorder. The only process state is `static CODES` (auth_flow.rs:154).
- fix: Create tests/it/main.rs with one `mod` per file and git mv the files into tests/it/.
- risk: Test names gain a module prefix. Update any `--test <name>` filters.

### MB-ADMIN-TESTS-11
- file: crates/mb-auth/Cargo.toml:44
- dimension: build-ci | guideline: tests:binary-consolidation | severity: low | semver: none | effort: S | classification: n/a
- evidence: `[[test]] name="oidc" required-features=["testing"]`. Naming a module `core` would make `core::` paths ambiguous.
- fix: Create tests/it/main.rs with `mod basics; #[cfg(feature = "testing")] mod oidc;` and delete the [[test]] block.
- risk: Low.

### MB-ADMIN-TESTS-12
- file: crates/mb-admin/tests/audit.rs:835
- dimension: tests | guideline: tests:shared-poll-helper | severity: low | semver: none | effort: S | classification: n/a
- evidence: `wait_for` returns whatever it has when it times out, and does not panic. There are hand-rolled poll loops at audit.rs:958 and imports.rs:988, 1086 and 1096, and they mix std and tokio Instant.
- fix: Add one `eventually(what, timeout, f)` helper that panics with a message on timeout.
- risk: Very low.

### MB-ADMIN-TESTS-13
- file: crates/mb-admin/tests/imports.rs:886
- dimension: tests | guideline: tests:resource-cleanup | severity: low | semver: none | effort: S | classification: n/a
- evidence: Each Postgres parity run executes `CREATE SCHEMA {schema}`, and nothing in the file runs DROP SCHEMA.
- fix: Drop the schema with CASCADE at the end of each test, or have CI use a throwaway database.
- risk: Low.

## Dropped
- MB-ADMIN-STUB-12 (openapi.rs:242): serialising the utoipa document to JSON cannot realistically fail, and tests/api.rs:1120 and auth_flow.rs:713 already fetch and inspect the document, so the `{}` fallback is not reachable in practice.
- MB-ADMIN-TESTS-6 (live.rs:336/374): the revoked-stream test is not trivially passing. The 250 ms sleep makes sure the recheck has run before s2 is sent, so the test asserts that the event is not delivered. Removing the sleep would make it flaky, because s2 could arrive before the 100 ms recheck, and the saving is about 0.4 s.
- MB-ADMIN-TESTS-7 (oidc.rs:356): saves about 0.1 s at the cost of tighter timing on a throttle that uses std Instant.
