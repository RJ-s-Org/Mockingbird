# Audit: mb-store, mb-llm, mb-ext, mb-console, mb-evals, custom-ext (verified)

Adversarial verification of 54 findings. 46 kept (some re-rated), 8 dropped (listed at the end).
Context that changes severity: `mockingbird-store`, `mockingbird-llm` and `mockingbird-console` are published as
"Internal crate ... no API stability guarantee" and pinned `=0.1.0` by the workspace, so API-polish items on them
are low priority. `mb-ext` 1.0.0 is the only semver-stable surface. `mb-evals` and `examples/custom-ext` are `publish = false`.

## High

### MB-STORE-STUB-2: Saving a profile in the console erases its extra_body
- file: crates/mb-console/web/src/pages/ProfileEditor.tsx:74
- dimension: stub; guideline: data-loss; severity: high; semver: none; effort: S; classification: finish
- evidence: The PUT body forwards `headers: profile?.headers ?? {}` (comment: kept as stored so a save does not drop them) but omits `extra_body`. `PutProfile.extra_body` is `#[serde(default)]` (mb-admin profiles.rs:117-119) and `put()` stores `body.extra_body` as given (profiles.rs:403), so any console edit resets OpenRouter `provider`/`reasoning` settings to `{}`.
- fix: Add `extra_body: profile?.extra_body ?? {}` next to `headers`, plus a ProfileEditor test that a save round-trips extra_body. Editors for headers/extra_body are a separate, optional follow-up.
- risk: Very low for the one-line fix.

## Medium

### MB-STORE-STUB-1: Admin API refuses vault: secret_ref for LLM profiles, though the Kubernetes docs say it works
- file: crates/mb-admin/src/profiles.rs:153
- dimension: stub; guideline: docs-vs-behaviour; severity: medium (was high: fails loudly with 422, no data loss); semver: none; effort: S; classification: finish
- evidence: `check_secret_ref` accepts only `env:`/`file:` before consulting the `api_secret_refs` globs. docs/deploy/kubernetes.md:172 tells users to allow `vault:secret/mockingbird*` in `api_secret_refs`. The runtime resolves profile refs through `deps.ext.secrets` (mockingbird runtime.rs:570), which handles vault:, so only the admin gate blocks it. Webhook secrets accept vault: (mb-store model.rs:214).
- fix: Accept a well-formed `vault:<mount>/<path>#<field>` in check_secret_ref (still gated by the globs); update doc comments at profiles.rs:43/150 and LlmProfile.secret_ref (model.rs:659); add an api.rs test that a well-formed vault ref passes when allowed. Keep `vault:x` rejected (malformed).
- risk: Low; the glob allow-list still gates it.

### MB-STORE-STUB-3: Streaming drops in-stream provider errors/truncation; FallbackProvider never fails over on an empty stream
- file: crates/mb-llm/src/openai.rs:409
- dimension: stub; guideline: error-handling; severity: medium; semver: none; effort: S; classification: finish
- evidence: In `OpenAiCompatible::stream`, any chunk without `choices[0].delta.content` hits `_ => continue`, so `{"error":...}`, `finish_reason:"error"` and `finish_reason:"length"` are silently dropped (parse_reply handles them for complete()). `FallbackProvider::stream` returns an empty stream when the primary yields nothing (fallback.rs:120).
- fix: Apply parse_reply's error/finish rules per chunk (error -> `LlmError::Http`, length -> `Truncated`); in FallbackProvider::stream treat an end-before-first-delta as failure and try the secondary. Wiremock tests in tests/openai.rs and tests/fallback.rs.
- risk: Streams that ended quietly now end with an Err item; only explicit error/finish markers should trigger (keep-alive chunks stay ignored).

### MB-STORE-STUB-4: ProviderPolicy documented as per-call and tenant-aware, but called once per engine build with tenant "default"
- file: crates/mb-ext/src/policy.rs:6
- dimension: api; guideline: C-STABLE / docs-vs-behaviour; severity: medium; semver: none; effort: L; classification: ask-user
- evidence: Trait doc: "Chooses the LLM profile a call may use"; mb-ext README: "when an LLM profile is chosen for a call". The only call site is mockingbird runtime.rs:477-480 (`choose_profile`) with `Tenancy::new("default", mock.workspace)`; a Rejection becomes an anyhow error that fails the build, so status/Retry-After are never served.
- fix: Product decision: (a) correct the trait doc and README (called at engine build, tenant `default`, rejection disables LLM for the mock), or (b) call per request after admission with the resolved tenancy and render Rejection as problem+json.
- risk: (a) docs only; (b) hot-path and behaviour change under mb-ext 1.x, needs a release note.

### MB-STORE-STUB-5: The Vault client ignores outbound.proxy / outbound.no_proxy / outbound.ca_file
- file: crates/mb-ext/src/vault.rs:112
- dimension: stub; guideline: docs-vs-behaviour; severity: medium; semver: additive; effort: M; classification: finish
- evidence: `VaultSecrets::from_vars` builds `reqwest::Client::builder().timeout(REQUEST_TIMEOUT)` plus VAULT_CACERT only; it never sees mb_llm::HttpOptions. docs/proxy.md ("Start serve from a clean shell") names Vault among the outbound clients and says to use `outbound.proxy`/`outbound.no_proxy`, which never reach Vault (reqwest still honours HTTPS_PROXY env).
- fix: Add an additive mb-ext constructor taking a plain-data options struct defined in mb-ext (proxy URL, no_proxy, extra CA PEM), wire it from the binary; or narrow docs/proxy.md to say Vault honours only HTTP(S)_PROXY env and VAULT_CACERT.
- risk: Additive in mb-ext 1.x; avoid exposing reqwest types in the public API.

### MB-STORE-PERF-1: list_contract_versions loads and decodes every version's full source and bundle; versions are never pruned
- file: crates/mb-store/src/sqlite.rs:794 (also postgres.rs:804)
- dimension: perf; guideline: PERF:unbounded-load; severity: medium; semver: additive (mb-store is internal 0.1); effort: M
- evidence: Selects VERSION_COLUMNS (includes `source`, `bundle`) with no LIMIT, then `serde_json::from_str` per bundle (sqlite.rs:302). Callers need little: the admin list handler (mb-admin contracts.rs:476) maps to `ContractVersionSummary`; the import worker (imports/worker.rs:255) uses only the newest Draft/Active row. No `DELETE FROM contract_versions` exists, so cost grows as O(#versions x bundle size) per console list.
- fix: Add trait methods with default impls: `list_contract_version_summaries(mock_id)` selecting metadata (plus operation/channel counts or stored counts) and `latest_editable_version(mock_id)` (`WHERE status IN ('draft','active') ORDER BY version DESC LIMIT 1`). Switch both callers. Consider pagination / a retention setting later.
- risk: Low; summary counts must match current `bundle.operations.len()` semantics; cover both backends in conformance.

### MB-STORE-STYLE-1: OpenAiConfig derives Debug, so {:?} prints the resolved API key and header values
- file: crates/mb-llm/src/openai.rs:11
- dimension: api; guideline: C-DEBUG; repo rule: secrets never logged; severity: medium; semver: none; effort: S
- evidence: `#[derive(Clone, Debug)] pub struct OpenAiConfig` with `pub api_key: Option<String>` and resolved `headers` (cf-aig-authorization, api-key). mb-ext hand-redacts its Debug impls (vault.rs:52, 243). No current `{:?}` use found, so this is latent.
- fix: Manual `impl fmt::Debug` redacting `api_key` and header values (names only); unit test that a sentinel key is absent from `format!("{cfg:?}")`.
- risk: Very low; Debug output only.

### MB-STORE-TESTS-2: About 62 conformance checks run serially inside one #[tokio::test] per backend
- file: crates/mb-store/tests/common/conformance.rs:21
- dimension: tests; guideline: test-isolation; severity: medium; semver: none; effort: M
- evidence: `run_all` awaits ~62 check fns in sequence; sqlite.rs:9 and postgres.rs:37 each wrap it in one test. The first panic hides all later failures; `drain_queue` exists because checks share one store.
- fix: Macro-generate one `#[tokio::test]` per check with a fresh store (sqlite::memory:, Postgres schema-per-test via url_with_fresh_schema). Keep `run_all` as a wrapper if wanted.
- risk: Some checks may depend on earlier state; audit each. 62 Postgres schemas each running migrations adds setup time.

### MB-STORE-TESTS-3: Postgres tests silently pass when MB_TEST_POSTGRES_URL is missing
- file: crates/mb-store/tests/postgres.rs:39 (also upgrade.rs:163)
- dimension: tests; guideline: no-silent-skip; severity: medium; semver: none; effort: S
- evidence: `println!("skipped ...")` and return, reported as passed. CI sets the var at ci.yml:56; renaming/dropping it would silently remove Postgres coverage.
- fix: Shared helper in tests/common that asserts `std::env::var_os("CI").is_none()` before skipping.
- risk: Low; forks without Postgres in CI must set or opt out.

## Low

### MB-STORE-PERF-2: No bulk active-contract query; callers do N+1 queries each decoding a full bundle
- file: crates/mb-store/src/lib.rs:106
- dimension: perf; guideline: PERF:n-plus-one; severity: low (was medium: control-plane only, on gRPC activation and reconcile); semver: additive; effort: M
- evidence: Only per-mock `active_contract`. mb-admin contracts.rs:439-446 (`ensure_grpc_services_free`) loops list_all_mocks + active_contract per mock to read gRPC service names; mockingbird reconcile.rs:451 loops similarly.
- fix: Add `list_active_contracts()` with a default loop impl, overridden with one `WHERE status='active'` query in both backends; for the gRPC check, a narrower service-names query avoids bundle decode.
- risk: Low; order not significant to callers.

### MB-STORE-PERF-3: last_json re-parses from every '{'/'[' and builds a full Value each time
- file: crates/mb-llm/src/openai.rs:324
- dimension: perf; guideline: PERF:quadratic; severity: low; semver: none; effort: S
- evidence: For each unconsumed bracket it runs a fresh `Deserializer::into_iter::<Value>()`; failed parses (unclosed draft JSON) don't advance `next`, so nested brackets rescan; worst case O(n^2) over ~10-20 KB reasoning. Only on the `content: null` reasoning-only path (parse_reply), which is rare and dwarfed by the LLM call itself.
- fix: Probe with `serde::de::IgnoredAny` (classify object/array from the first char) and only build/borrow the slice on success; optionally skip past consumed bytes on failure.
- risk: Must keep trailing-value and nested-skip semantics; the reasoning-only tests in tests/openai.rs cover it.

### MB-STORE-PERF-4: Console copies each embedded asset into a fresh Vec on every request
- file: crates/mb-console/src/lib.rs:90
- dimension: perf; guideline: PERF:needless-copy; severity: low (was medium: admin-port only, hashed assets are immutable-cached); semver: none; effort: S
- evidence: `Body::from(file.data.into_owned())`; in release rust-embed data is `Cow::Borrowed(&'static [u8])`, so each GET memcpys the file. index.html is served `no-cache` without an ETag.
- fix: `Body::from(file.data)` (axum Body implements `From<Cow<'static, [u8]>>`) or match Borrowed -> `Bytes::from_static`. Optionally add ETag from `file.metadata.sha256_hash()` with 304 for no-cache entries.
- risk: Minimal; ETag/304 needs a test in tests/serve.rs.

### MB-STORE-PERF-5: PrometheusUsage allocates ~8 Strings and does 3-4 registry lookups per data-plane call
- file: crates/mb-ext/src/usage.rs:72
- dimension: perf; guideline: PERF:hot-path-alloc; severity: low; semver: none; effort: S
- evidence: `record()` clones `mock` up to 4x, `operation_id` 2x, `source` 1x, `status.to_string()`, and each `metrics!` macro hashes the label set. Runs per completed call (including non-LLM answers).
- fix: Cache `(Counter, Histogram)` handles inside PrometheusUsage keyed by (mock, operation, status, source); no public type change.
- risk: Low; metric names/labels must stay identical (mb-ext defaults test).

### MB-STORE-PERF-7: SQLite request-log prune uses `id NOT IN (subquery LIMIT retention)`
- file: crates/mb-store/src/sqlite.rs:1035
- dimension: perf; guideline: PERF:query-shape; severity: low; semver: none; effort: S
- evidence: SQLite: `DELETE ... WHERE mock_id=?1 AND id NOT IN (SELECT id ... ORDER BY id DESC LIMIT ?2)`. Postgres (postgres.rs:1059) uses `id <= (SELECT id ... OFFSET retention LIMIT 1)`, an index seek plus range delete on `request_log_mock_id (mock_id, id)`.
- fix: Use the Postgres shape in SQLite: `... AND id <= (SELECT id FROM request_log WHERE mock_id=?1 ORDER BY id DESC LIMIT 1 OFFSET ?2)`. A NULL subquery (fewer rows than retention) deletes nothing, same as today.
- risk: Very low; conformance covers retention on both backends.

### MB-STORE-PERF-8: Vault cache has no single-flight on a miss and never evicts expired entries
- file: crates/mb-ext/src/vault.rs:202
- dimension: perf; guideline: PERF:thundering-herd; severity: low; semver: none; effort: S
- evidence: `resolve()` checks `cached()`, then `fetch()` uncoordinated, so concurrent misses after the 5-min TTL each hit Vault. Expired entries are only overwritten on reinsert. The std Mutex is not held across `.await`.
- fix: Per-key `tokio::sync::OnceCell`/Mutex so one fetch serves concurrent misses; `retain` expired entries on insert. Do not cache errors.
- risk: Low.

### MB-STORE-PERF-9: Example FixedQuota keys an unbounded DashMap by a client-supplied header
- file: examples/custom-ext/src/lib.rs:69
- dimension: perf; guideline: PERF:unbounded-collection; severity: low; semver: none; effort: S
- evidence: `self.counts.entry(t.tenant.clone())` per call; tenant comes straight from `X-Tenant` (lines 30-38), so any client can grow `counts` without bound. The doc comment mentions shared storage but not authentication; this is the documented starting point for extension authors.
- fix: Comment that a real resolver must authenticate tenants before they become keys (or cap tenant length/count). Optionally `get_mut(t.tenant.as_str())` first, `entry()` on miss.
- risk: None; get_mut holds the shard lock, so check-and-increment stays atomic.

### MB-STORE-STYLE-2: mb-ext 1.0 public type Extensions does not implement Debug
- file: crates/mb-ext/src/lib.rs:122
- dimension: api; guideline: C-DEBUG / C-COMMON-TRAITS; severity: low; semver: additive; effort: S
- evidence: `#[derive(Clone)] #[non_exhaustive] pub struct Extensions` holds six `Arc<dyn Trait>`; every other mb-ext public type has Debug. Downstream structs holding Extensions cannot derive Debug.
- fix: `impl fmt::Debug for Extensions` with `finish_non_exhaustive()`; add `#![warn(missing_debug_implementations)]`. Do not add a Debug supertrait (2.0 break).
- risk: None.

### MB-STORE-STYLE-3: Rejection is the Err type of three stable traits but implements neither Display nor Error
- file: crates/mb-ext/src/lib.rs:68
- dimension: api; guideline: C-GOOD-ERR; severity: low (was medium: ergonomics); semver: additive; effort: S
- evidence: Used as Err in TenantResolver::resolve, QuotaGuard::admit, ProviderPolicy::choose; derives only Debug, Clone, PartialEq, Eq.
- fix: `impl fmt::Display` (e.g. `"{status} {title}: {detail}"`) and `impl std::error::Error for Rejection {}`.
- risk: Additive; negligible overlap risk with downstream blanket impls.

### MB-STORE-STYLE-4: mb-ext crate docs have no rustdoc examples
- file: crates/mb-ext/src/lib.rs:1
- dimension: api; guideline: C-CRATE-DOC / C-EXAMPLE; severity: low (was medium); semver: none; effort: M
- evidence: No code blocks in crates/mb-ext/src. The README does contain an `Extensions::default()` example (shown on crates.io) but it is not included in the crate docs, so docs.rs shows none and it is not doctested.
- fix: Add an `# Example` to the crate doc (small QuotaGuard impl + `Extensions::default().with_quota(..)`), or `#![doc = include_str!("../README.md")]`; short examples on `Rejection::new`, `Tenancy::new`, `DefaultSecrets::from_vars` (closure over a fixed map).
- risk: Docs only; doctests become a useful semver tripwire.

### MB-STORE-STYLE-5: Fallible mb-ext functions and SecretStore::resolve lack `# Errors` sections
- file: crates/mb-ext/src/vault.rs:88 (also vault.rs:94, secrets.rs:17, secrets.rs:36)
- dimension: api; guideline: C-FAILURE; severity: low; semver: none; effort: S
- evidence: `VaultSecrets::from_env/from_vars` and `check_secret_file` describe failures in prose; `SecretStore::resolve` never states the SecretNotFound / BadSecretRef / Other mapping implementers should follow.
- fix: Add `/// # Errors` sections listing variants; consider `#![warn(clippy::missing_errors_doc)]` on mb-ext.
- risk: Docs only; documents current behaviour as the 1.x contract.

### MB-STORE-STYLE-6: Stringly fields in the stable surface: Rejection.status is u16, UsageEvent.source is a String with a closed value set
- file: crates/mb-ext/src/lib.rs:70 (also usage.rs:27)
- dimension: api; guideline: C-CUSTOM-TYPE; severity: low; semver: additive; effort: S
- evidence: `pub status: u16` while `http` 1 is already public API; `UsageEvent.source` is documented as one of llm/repaired/fallback/not_generated/import, so sinks string-match.
- fix: Keep fields (changing them is mb-ext 2.0). Add `Rejection::from_status(http::StatusCode, ..)` and `UsageEvent::SOURCE_*` consts used by the core. Track typed fields for 2.0.
- risk: None for additive helpers.

### MB-STORE-STYLE-8: `reasoning_off(self, on: bool)` is a confusing bool builder
- file: crates/mb-llm/src/openai.rs:53
- dimension: api; guideline: Google:enum-over-bool / naming; severity: low; semver: additive (mb-llm internal 0.1); effort: S
- evidence: `.reasoning_off(false)` means reasoning allowed; siblings use `with_*`. Only callers are tests (tests/openai.rs:413, 440, 469).
- fix: Add `#[must_use] with_reasoning_disabled(self, disabled: bool)` (or an enum), deprecate `reasoning_off`. Given mb-llm is internal with only test callers, a direct rename is also acceptable.
- risk: Low.

### MB-STORE-STYLE-9: By-value builder methods in mb-llm lack #[must_use]
- file: crates/mb-llm/src/fallback.rs:53 (also fallback.rs:59, openai.rs:53, scripted.rs:59, scripted.rs:65)
- dimension: lint; guideline: Google:must-use; severity: low; semver: additive; effort: S
- evidence: These take `self` and return `Self` with no `#[must_use]`; mb-ext marks all its builders.
- fix: Add `#[must_use]` to each.
- risk: Only adds warnings to already-buggy downstream code.

### MB-STORE-STYLE-10: mb-llm and mb-store public items largely undocumented; no missing_docs lint
- file: crates/mb-llm/src/lib.rs:22 (also mb-store lib.rs, error.rs, model.rs)
- dimension: api; guideline: Google:public-docs; severity: low; semver: none; effort: M
- evidence: Undocumented: Role, ChatMessage, ResponseFormat, ChatRequest, Usage, ChatResponse, Capabilities, LlmError and some variants, Provider, OpenAiConfig fields, OpenAiCompatible, FallbackProvider::new; StoreError and variants, many ControlStore methods, PgStore, SqliteControlStore, model enums. Neither crate has `missing_docs` (mb-ext has `deny`). Both crates are internal, which lowers priority.
- fix: Fill gaps (one-liners where obvious) and add `#![warn(missing_docs)]` in the same PR (CI runs `-D warnings`).
- risk: Docs only; lint fails CI until gaps are filled.

### MB-STORE-STYLE-11: Public validators return Result<(), String>
- file: crates/mb-store/src/model.rs:175 (also 223, 342, 383, 423, 491, 614)
- dimension: api; guideline: C-GOOD-ERR; severity: low (was medium: internal crate); semver: breaking-needs-0.2; effort: M
- evidence: Seven pub validators return `Result<(), String>`; elsewhere `validate_*` in lib.rs return `Result<(), StoreError>`, wrapped via `.map_err(StoreError::Invalid)`.
- fix: In 0.2 change to `Result<(), StoreError>` (Invalid); or add StoreError-returning variants now and deprecate. Keep message text (tests assert substrings like `webhooks[0].url`).
- risk: Breaking for external callers of these pub fns.

### MB-STORE-STYLE-12: Field-less mb-store enums miss Copy, Eq and Hash
- file: crates/mb-store/src/model.rs:535 (also 928, 58, 21)
- dimension: api; guideline: C-COMMON-TRAITS; severity: low; semver: additive; effort: S
- evidence: ContractStatus, ImportJobStatus, ResourceSource, DataPlaneAuthMode derive only Clone, Debug, PartialEq (+Default); sibling unit enums WebhookSigning, RequestKind, Consistency, UnknownIds derive Copy, Eq.
- fix: Add `Copy, Eq, Hash`; remove now-needless `.clone()` calls in the same PR (clippy clone_on_copy under -D warnings).
- risk: Additive.

### MB-STORE-STYLE-13: Ad-hoc as_str/parse pairs where Display/FromStr would fit
- file: crates/mb-store/src/model.rs:704 (also 1007, 1026, 1049; mb-llm lib.rs:30)
- dimension: api; guideline: C-CONV-TRAITS; severity: low; semver: additive; effort: S
- evidence: `RequestKind::parse -> Option<Self>` is pub; ResourceSource/ContractStatus/ImportJobStatus have `as_str` plus `pub(crate) parse -> Result<_, StoreError>`; none implement Display or FromStr; `mb_llm::Role::as_str` likewise.
- fix: `impl Display` delegating to `as_str`; `impl FromStr` (Err = StoreError) for the mb-store enums, with existing `parse` delegating.
- risk: Additive.

### MB-STORE-STYLE-14: StoreError and LlmError are exhaustive and flatten sources to String
- file: crates/mb-store/src/error.rs:1 (also mb-llm lib.rs:92)
- dimension: api; guideline: C-GOOD-ERR / non-exhaustive-enums; severity: low; semver: breaking-needs-0.2; effort: M
- evidence: `StoreError` has no `#[non_exhaustive]`; `From<sqlx::Error>`/`MigrateError`/`serde_json::Error` stringify, so `source()` is None. LlmError likewise. mb-ext ExtError is non_exhaustive.
- fix: At 0.2 add `#[non_exhaustive]` and keep a `#[source]` box in `Db`; keep Display text unchanged (problem+json may surface it).
- risk: Breaks downstream exhaustive matches; 0.2 only.

### MB-STORE-STYLE-15: Public store and provider types lack Debug; store types are named inconsistently
- file: crates/mb-store/src/postgres.rs:73 (also sqlite.rs:55, mb-llm openai.rs:27, fallback.rs:35, scripted.rs:16)
- dimension: api; guideline: C-DEBUG; severity: low; semver: additive; effort: S
- evidence: No Debug on PgStore, SqliteControlStore, OpenAiCompatible, FallbackProvider, ScriptedProvider; backends named `PgStore` vs `SqliteControlStore`.
- fix: Manual Debug with `finish_non_exhaustive()` (never print a URL that may embed a password; OpenAiCompatible delegates to the redacted OpenAiConfig Debug from STYLE-1). Naming: pick one scheme at 0.2; an alias is optional.
- risk: Additive.

### MB-STORE-STYLE-17: Single-letter parameter names in stable trait signatures and builders
- file: crates/mb-ext/src/quota.rs:12 (also policy.rs:13, lib.rs:93, lib.rs:140-175)
- dimension: style; guideline: Google:no-abbreviations; severity: low; semver: none; effort: S
- evidence: `admit(&self, t: &Tenancy, ..)`, `choose(&self, t: &Tenancy, ..)`, `with_tenants(t)`, `with_quota(q)`, ..., `with_retry_after(d)` appear verbatim in rustdoc and IDE-generated impl stubs.
- fix: Rename to `tenancy`, `resolver`, `guard`, `sink`, `store`, `policy`, `hooks`, `delay`.
- risk: Parameter names are not part of Rust semver; cosmetic.

### MB-STORE-STUB-6: Unconfigured or misconfigured Vault is reported as "invalid secret reference" on every resolve
- file: crates/mb-ext/src/vault.rs:309
- dimension: stub; guideline: error-reporting; severity: low (was medium); semver: none; effort: S; classification: finish
- evidence: When VAULT_ADDR is unset or the config is unusable, `DefaultSecrets::resolve` returns `ExtError::BadSecretRef(reference)` (Display: `invalid secret reference "vault:..."`), pointing at a well-formed reference; the real cause is logged once (line 293). This mapping is documented on DefaultSecrets (vault.rs:228-231) and asserted by tests (tests/vault.rs, defaults.rs:74, default_extensions_vault_env.rs:42).
- fix: Store the init error in the OnceCell and return `ExtError::Other("vault references need VAULT_ADDR")` / `Other("Vault disabled: <reason>")`, keeping BadSecretRef for malformed refs. Update the doc at vault.rs:228-231 and the tests.
- risk: Changes a documented 1.x behaviour (which variant is returned); ExtError is non_exhaustive and no workspace caller branches on BadSecretRef, but ship with a release note.

### MB-STORE-STUB-7: Secret-reference validator and messages advertise `vault:path#key`, which the Vault parser rejects
- file: crates/mb-store/src/model.rs:195 (validator at 214)
- dimension: stub; guideline: validate-at-write; severity: low; semver: none; effort: S; classification: finish
- evidence: `is_secret_reference` accepts any non-blank `vault:` remainder; the message says `vault:path#key`. mb-ext `parse_ref` (vault.rs:72) requires `vault:<mount>/<path>#<field>`, so e.g. `vault:token#key` passes validation and fails only at delivery.
- fix: Check the mount/path#field shape in `is_secret_reference`; change all messages/docs to `vault:<mount>/<path>#<field>`; list vault in LlmProfile.secret_ref doc (model.rs:659).
- risk: A stored malformed vault ref would fail validation on next update; it never worked.

### MB-STORE-STUB-9: Admin API routes with no console UI (workspace update/delete, webhook deliveries, mock hosts/webhooks/behavior)
- file: crates/mb-console/web/src/api/queries.ts:18
- dimension: stub; guideline: product-scope; severity: low; semver: none; effort: M; classification: ask-user
- evidence: queries.ts only GETs/POSTs `/workspaces`; nothing calls `webhooks/deliveries`; no page edits hosts/webhooks/behavior. docs/console.md:35 lists "owner: also update or delete the workspace" in the role table.
- fix: Product call. Suggested order: read-only webhook deliveries table; workspace rename/delete for owners; hosts/webhooks/behavior fields in Overview.tsx.
- risk: UI only; webhook secrets must stay reference-only.

### MB-STORE-STUB-10: Streamed calls charge estimated tokens instead of provider-reported usage
- file: crates/mb-engine/src/stream.rs:128 (provider side: crates/mb-llm/src/openai.rs, stream())
- dimension: stub; guideline: billing-accuracy; severity: low; semver: additive; effort: M; classification: finish
- evidence: `TextStream` carries text only, so StreamMeter estimates `ceil(bytes/4)` (documented in the comment). No `stream_options`/`include_usage` anywhere in crates/; the final usage chunk is dropped. UsageSink consumers (cloud billing) get approximate numbers for streamed calls.
- fix: Send `stream_options.include_usage` (retry without it if rejected, like the reasoning switch); expose the final usage via an additive Provider method with an estimating default; prefer reported usage in StreamMeter.
- risk: Some OpenAI-compatible servers reject unknown fields; keep the API additive.

### MB-STORE-STUB-11: Evals skip streaming operations and do not report skipped contracts or calls
- file: crates/mb-evals/src/report.rs:186
- dimension: stub; guideline: coverage-visibility; severity: low; semver: internal-only; effort: M; classification: finish
- evidence: `targets()` filters out streaming ops (also bench.rs:51); contracts that fail to compile (corpus.rs:130) and rejected calls (report.rs:271) are only `tracing::warn`ed and absent from report.json/markdown, so a shrinking corpus still passes thresholds.
- fix: Add `skipped_contracts`/`skipped_calls` to EvalReport and to_markdown (optional threshold); add a streaming target class. mb-evals is publish=false.
- risk: CI eval runs may start flagging previously silent skips.

### MB-STORE-STUB-12: is_placeholder() is never used by the binary; a source build silently serves the "Console not built" page
- file: crates/mb-console/src/lib.rs:48
- dimension: stub; guideline: operability; severity: low; semver: none; effort: S; classification: finish
- evidence: Only crates/mb-console/tests/serve.rs calls it; nothing in crates/*/src. A local/`--git` build without the Vite build logs nothing at startup.
- fix: At serve startup, warn when `mb_console::is_placeholder()` naming the build command; optionally surface in `mockingbird doctor`.
- risk: None.

### MB-STORE-STUB-13: Stale module doc says cross-node LISTEN/NOTIFY is still to come
- file: crates/mb-store/src/postgres.rs:4
- dimension: stub; guideline: stale-docs; severity: low; semver: none; effort: S; classification: finish
- evidence: "cross-node LISTEN/NOTIFY arrives with HA"; docs/architecture.md:69 describes the shipped bus (`bus.url`, Postgres LISTEN/NOTIFY or Redis).
- fix: Reword: subscribe() is in-process; the mb-state bus relays changes between nodes and receivers reload from the store.
- risk: None.

### MB-STORE-TESTS-1: bench_offline_smoke uses a 1s wall-clock load window with 4 workers
- file: crates/mb-evals/tests/evals.rs:269
- dimension: tests; guideline: test-wall-time; severity: low (was medium); semver: none; effort: S
- evidence: `bench(.., Duration::from_secs(1), 4)` loops until the deadline; assertions only need requests>0, errors==0, percentile ordering. Must stay wall-clock (real sockets), but the window can shrink.
- fix: Use 150-250 ms. Saves ~0.8 s per run plus CPU contention.
- risk: Low; the assertion needs only one request.

### MB-STORE-TESTS-4: run_without_env_is_usage_error asserts only exit code 2, which clap parse errors also return
- file: crates/mb-evals/tests/evals.rs:262
- dimension: tests; guideline: assertion-strength; severity: low (was medium); semver: none; effort: S
- evidence: main.rs calls `Cli::parse()` (exits 2 on any arg error) and the missing-env path also returns EXIT_USAGE=2, so a broken `run --corpus` CLI still passes.
- fix: Also assert stderr contains the from_env error text (e.g. `MB_EVAL_LLM_BASE_URL`) via `predicates::str::contains`.
- risk: None.

### MB-STORE-TESTS-5: sqlite_migration_backfills_workspaces duplicates upgrade.rs sqlite_upgrades_from_p2
- file: crates/mb-store/tests/sqlite.rs:55
- dimension: tests; guideline: test-duplication; severity: low; semver: none; effort: S
- evidence: Both copy 0001_init.sql, run a Migrator with `_mb_store_migrations`, insert legacy rows and reconnect; upgrade.rs:117-157 generalises it. sqlite.rs uniquely checks the `profiles-only` workspace backfill and default data_plane_auth.
- fix: Move the unique assertions (and the llm_profiles legacy row, valid for both backends or sqlite-only) into upgrade.rs, then delete the sqlite.rs copy.
- risk: Low.

### MB-STORE-TESTS-9: ScriptedProvider test sits in the openai test binary behind #[cfg(feature = "testing")]
- file: crates/mb-llm/tests/openai.rs:168
- dimension: tests; guideline: test-organisation; severity: low; semver: none; effort: S
- evidence: `scripted_returns_in_order` is cfg-gated inside openai.rs, which has no required-features; without --all-features it silently vanishes. tests/scripted.rs already requires `testing`.
- fix: Move it into tests/scripted.rs and drop the cfg.
- risk: None.

### MB-STORE-TESTS-10: mb-store's 3 test binaries can merge into tests/it/main.rs
- file: crates/mb-store/tests/upgrade.rs:1
- dimension: tests; guideline: build-time / test-binary-consolidation; severity: low; semver: none; effort: S
- evidence: sqlite.rs, postgres.rs, upgrade.rs each link sqlx/tokio/mb-store; postgres.rs schema helpers are duplicated in upgrade.rs; upgrade.rs has a crate-level `#![allow(clippy::unwrap_used)]`. No env set_var, fixed ports or global subscribers.
- fix: `tests/it/main.rs` with `mod common; mod sqlite; mod postgres; mod upgrade;`, hoist PG schema helpers into common, move the allow to module level.
- risk: Low; saves two links per build (not measured).

### MB-STORE-TESTS-11: mb-ext can merge defaults.rs and vault.rs; default_extensions_vault_env.rs must stay separate
- file: crates/mb-ext/tests/default_extensions_vault_env.rs:19
- dimension: tests; guideline: test-binary-consolidation; severity: low; semver: none; effort: S
- evidence: default_extensions_vault_env.rs calls `unsafe std::env::set_var` for VAULT_* and relies on being alone in its process (comment lines 16-17). defaults.rs and vault.rs avoid set_var (defaults.rs:43) and use thread-local recorders/subscribers.
- fix: Merge defaults.rs + vault.rs into tests/it/main.rs (`mod support; mod defaults; mod vault;`); keep the env test as its own binary.
- risk: Low; saves one link.

### MB-STORE-TESTS-12: mb-llm consolidation blocker: required-features = ["testing"] on fallback and scripted only
- file: crates/mb-llm/Cargo.toml:37
- dimension: tests; guideline: test-binary-consolidation; severity: low; semver: none; effort: S
- evidence: `[[test]] fallback` and `scripted` require `testing`; openai does not.
- fix: Single `[[test]] name = "it"` with no required-features and `mod openai; #[cfg(feature = "testing")] mod fallback; #[cfg(feature = "testing")] mod scripted;`. Do together with TESTS-9.
- risk: Same compiled-out behaviour as today without --all-features.

### MB-STORE-TESTS-13: custom-ext tests check status codes only and never shut down their servers
- file: examples/custom-ext/tests/ext.rs:35
- dimension: tests; guideline: assertion-strength; severity: low; semver: none; effort: S
- evidence: `serve()` binds 127.0.0.1:0 and spawns `axum::serve` with no shutdown; 200 responses are never inspected, so a wrong route answering 200 passes. Only the 429 case checks retry-after.
- fix: On the first 200, assert the body parses as the orders fixture shape; assert the 429 body is application/problem+json. Optionally drive `app.data` with `tower::ServiceExt::oneshot` if no extension needs ConnectInfo.
- risk: oneshot skips the network layer.

### MB-STORE-TESTS-14: Reserved-path test omits readyz and metrics; placeholder test never runs in CI
- file: crates/mb-console/tests/serve.rs:68
- dimension: tests; guideline: coverage; severity: low; semver: none; effort: S
- evidence: RESERVED (src/lib.rs:27) is [api, auth, healthz, readyz, metrics]; the loop covers /api, /auth, /healthz only. CI builds the console before tests, so `placeholder_mentions_build_command` always skips there; locally `hashed_asset_immutable_cache` skips.
- fix: Loop over every RESERVED entry; add a src-side `#[cfg(test)]` unit test of the placeholder marker check.
- risk: None.

## Dropped

- MB-STORE-PERF-6: `req.clone()` and Value body building cost microseconds next to a seconds-long LLM HTTP call; the fix needs public type changes for no measurable gain.
- MB-STORE-PERF-10: one small String allocation per proxied request on a path that does network I/O to an upstream; negligible.
- MB-STORE-STYLE-7: axum 0.8 in AdminHooks::routes is an explicit, documented decision in the mb-ext stability policy (lib.rs:24-26); the "breaking-needs-0.2" label is wrong for a 1.x crate; nothing to do until 2.0.
- MB-STORE-STYLE-16: the explicit `allow(clippy::unwrap_used)` attributes are correct and local; a workspace clippy.toml does not cover non-#[test] helpers in tests/ files, so it would churn without benefit.
- MB-STORE-STUB-8: refusing aws:/gcp:/azure: is a documented decision; changing the message requires changing the BadSecretRef variant/payload that tests and the 1.x contract assert, for a small hint.
- MB-STORE-TESTS-6: shrinking 3 ms/20 ms sleeps saves under 0.1 s and adds flake risk on coarse-timer platforms.
- MB-STORE-TESTS-7: the test already finishes at the 100 ms timeout; shrinking saves about 50 ms.
- MB-STORE-TESTS-8: the second response is a 200 success, so the client cannot retry "many" times; the test already proves exactly one retry by succeeding.
