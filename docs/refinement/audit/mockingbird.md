# Audit: mockingbird (mockingbird-cli 0.1.0, lib `mockingbird`)

Verified adversarially against the source at `claude/refinement-636e06` (read-only; no cargo/npm run).
55 findings in, 50 kept (some merged), 5 dropped. Paths are relative to `crates/mockingbird/` unless they start with `crates/`, `docs/` or `Cargo.toml`.

Merges: STYLE-5 into STYLE-4 (same clippy.toml fix), STYLE-9 into STYLE-8 (docs gaps), STYLE-13 and STYLE-15 into STYLE-1 (both are consequences of the whole CLI being public API), the claims_host half of STYLE-11 into PERF-2. Dropped: STUB-12 (deliberate, documented refusal; no defect).

## Performance

### MOCKINGBIRD-PERF-1: control-change watcher rebuilds a mock once per store write
- File: src/registry.rs:357 (spawn_watcher loop; reload_mock at :244)
- Dimension: perf. Guideline: PERF:coalesce-change-events
- Severity: medium (was high: control-plane path, not per request). Semver: none. Effort: S. Classification: n/a
- Evidence: each `ControlChange::Mock` runs a full `reload_mock`. mb-store publishes one per write: update_mock (twice when renamed), add_contract_version, activate_contract_version, each put/delete_scenario, put_event_settings and each put/delete_channel_settings (crates/mb-store/src/sqlite.rs:701-1215). One reconcile of a changed mock therefore triggers 3+k+c full rebuilds (contract load, store queries, router, reqwest clients, channel restart). Correction to the original: only the first event after a prefix/hosts change runs reload_all; later events see the new prefix already mounted.
- Fix: after the first `recv()`, drain with `try_recv()` (optionally after a ~50 ms debounce) into a `BTreeSet<(workspace, name)>` plus a resync flag; then run `reload_all` once if flagged, else `reload_mock` once per key.
- Risk: low. Rebuilds read the latest store state, so collapsing duplicates loses nothing; a debounce adds tens of ms of propagation delay.

### MOCKINGBIRD-PERF-2: claims_host builds and sorts every claimed host per proxied request
- File: src/registry.rs:403 (claimed_hosts at :410; callers src/proxy/server.rs:119, src/proxy/mod.rs:108)
- Dimension: perf. Guideline: PERF:hot-path-alloc
- Severity: low (was medium: H is small in practice and the proxy forwards over the network). Semver: none. Effort: S. Classification: n/a
- Evidence: `claims_host` calls `claimed_hosts()`, which clones every mounted and configured host into a `BTreeSet<String>`, collects a Vec, then scans linearly; it takes the `configured_hosts` mutex. Runs on every plain proxied request and every CONNECT. (Merged from STYLE-11's claims_host point.)
- Fix: keep an `ArcSwap<HashSet<String>>` recomputed wherever mounted or configured_hosts change (publish, reload_all_locked, set_configured_hosts); `claims_host` becomes load + contains. `claimed_hosts()` sorts a copy for /proxy/hosts.
- Risk: every writer must refresh the set or the proxy fails open/closed for a stale host; add a test for the reload_mock path. Preserve fail-closed semantics (hosts of unmounted mocks still count).

### MOCKINGBIRD-PERF-3: each logged request is deep-cloned for the live tail even with no subscribers
- File: src/recorder.rs:369
- Dimension: perf. Guideline: PERF:needless-clone
- Severity: medium. Semver: none. Effort: S. Classification: n/a
- Evidence: `let _ = self.live.send(r.clone());` runs unconditionally in `Writer::write`; the clone (request and response JSON up to 64 KiB each after truncation) is discarded when nobody tails. `Remote::relay` (:239) already checks `receiver_count()`.
- Fix: `if self.live.receiver_count() > 0 { let _ = self.live.send(r.clone()); }`.
- Risk: negligible; a subscriber joining between check and send misses one record, which a best-effort tail allows.

### MOCKINGBIRD-PERF-4: redaction and truncation scale with full, uncapped body size on the single writer task
- File: src/recorder.rs:356 (redact :455, normalize_key :489, truncate_body :425)
- Dimension: perf. Guideline: PERF:body-size-scaling
- Severity: medium. Semver: none. Effort: M. Classification: n/a
- Evidence: `write()` redacts the whole request and response, then `truncate_body` runs `serde_json::to_string` on the whole Value only to measure it. Only request bodies get a raw-size pre-cap in `record()`; responses do not. `normalize_key` allocates a String per object key at every depth, and `redact()` re-normalizes the configured fields on each call. A multi-MB response stalls logging for every mock (see PERF-5).
- Fix: normalize `redact_fields` once in `set_settings`/`new`; compare keys without allocating (char-filter iterator or reused scratch String); measure with `serde_json::to_writer` into a byte-counting writer, and for oversized bodies serialize into a writer that stops after PREVIEW_BYTES. Optionally have mb-protocols report response size as it does `request_body_bytes`.
- Risk: output must stay byte-identical (preview still redacted first); existing redact/truncate tests cover it.

### MOCKINGBIRD-PERF-5: request log written by one serial task, one INSERT round trip per entry
- File: src/recorder.rs:346 (Writer::run select loop; append_request_log at :391)
- Dimension: perf. Guideline: PERF:n-plus-1-writes
- Severity: medium. Semver: additive (new default method on mb-store's `ControlStore`; none for mockingbird-cli). Effort: M. Classification: n/a
- Evidence: the writer awaits `append_request_log` (insert plus periodic prune) for each record before reading the next, so throughput is capped near 1/store-RTT; above that the 4096-entry queue fills and `record()` drops entries (`mb_request_log_dropped_total`).
- Fix: drain with `rx.recv_many(&mut buf, 256)`; add `ControlStore::append_request_logs(Vec<_>, retention)` with a looping default impl, overridden by multi-row INSERT ... RETURNING in sqlite/postgres.
- Risk: medium. Live tail and bus announcements are delayed by up to one batch; prune/retention must stay per mock; the trait method needs a default to stay non-breaking for mb-store.

### MOCKINGBIRD-PERF-6: api_key data-plane auth hits the control store on every request
- File: src/dp_keys.rs:26 (crates/mb-auth/src/apikey.rs:185)
- Dimension: perf. Guideline: PERF:per-request-store-query
- Severity: medium. Semver: none. Effort: M. Classification: n/a
- Evidence: `StoreDataPlaneKeys::verify` calls `ApiKeys::verify`, which runs `store.get_api_key(key_id)` per request (plus `touch_api_key` every 60 s). Called from mb-protocols `verify_key` for every request on a mock in `api_key` mode, so control-DB load and latency track data-plane rps.
- Fix: short-TTL cache (10-30 s, bounded) keyed by a hash of the presented token, storing (workspace, sysadmin, expires_at); cache successes only; invalidate on revoke if a change is published; keep the throttled touch.
- Risk: a revoked key stays valid up to the TTL unless revocation invalidates the cache; document it.

### MOCKINGBIRD-PERF-7: every mock build creates its own reqwest Clients
- File: src/runtime.rs:590 (OpenAiCompatible::new; webhook client at :545)
- Dimension: perf. Guideline: PERF:client-reuse
- Severity: low (was medium: rebuilds are infrequent; the cost is lost connection reuse). Semver: additive (needs a new mb-llm constructor). Effort: M. Classification: n/a
- Evidence: `OpenAiCompatible::new` calls `crate::http_client(&config.http, None)` (crates/mb-llm/src/openai.rs:38) per hop per mock; `webhook_dispatcher` builds another Client per mock. M mocks on one provider hold M pools; each rebuild (PERF-1, PERF-8) discards the warm pool.
- Fix: build one Client per distinct (HttpOptions, redirect policy) in RuntimeDeps or a lazy cache; add `OpenAiCompatible::with_client(config, client)` to mb-llm.
- Risk: low; pool limits become shared across mocks.

### MOCKINGBIRD-PERF-8: reload_all rebuilds every mock serially and runs on any profile/workspace change
- File: src/registry.rs:214 (loop in reload_all_locked; trigger at :361-364)
- Dimension: perf. Guideline: PERF:serial-n-plus-1
- Severity: medium. Semver: none. Effort: M. Classification: n/a
- Evidence: `reload_all_locked` awaits `build_mounted` per mock in turn, under the reload mutex. `spawn_watcher` maps every Profile and Workspace change, Resync and Lagged to `reload_all`. The collision check `next.iter().find(collides)` is O(M^2).
- Fix: (a) on `Profile{workspace,name}` rebuild only mocks in that workspace whose profile (or the default) chain includes it, falling back to reload_all when unsure; (b) build with `futures::stream::iter(..).map(build).buffered(8)`, keeping order so collision resolution stays deterministic; (c) optionally index collisions by prefix.
- Risk: medium for (a) (a missed dependent keeps a stale profile); (b) raises peak store concurrency.

### MOCKINGBIRD-PERF-9: every reconcile recompiles every contract before checking its digest
- File: src/reconcile.rs:351 (digest at :426, comparison at :577-581)
- Dimension: perf. Guideline: PERF:recompile-unchanged
- Severity: low (was medium: reconcile runs on startup and --watch edits, not per request). Semver: additive (optional mb-store method). Effort: M. Classification: n/a
- Evidence: `validate()` runs `mb_import::compile` for every mock on every reconcile; the source digest is compared with the active version only later in apply, which loads the whole `active_contract` just to read `source_digest`.
- Fix: hash the source first; if it equals the active version's digest, validate against the stored bundle and skip compile. Optionally add `active_contract_digest(mock_id)` with a default impl.
- Risk: low; the digest-equal path already keeps the stored version.

### MOCKINGBIRD-PERF-10: proxy keeps every unmapped host forever, in a set and as a metric label
- File: src/proxy/mod.rs:113 (seen set at :57, first_sighting :86)
- Dimension: perf. Guideline: PERF:unbounded-cardinality
- Severity: low (was medium: dev proxy, small per-host cost, but unbounded). Semver: none. Effort: S. Classification: n/a
- Evidence: `note_unmapped` labels `mb_proxy_unmapped_requests_total` with `host`, and `first_sighting` inserts into `seen: Mutex<HashSet<String>>`, never pruned. A browser routed through the proxy adds a permanent entry and Prometheus series per third-party host.
- Fix: cap distinct host labels (e.g. 500) and count the rest under `host="other"`, logging once; bound `seen` the same way.
- Risk: `mockingbird report` aggregates hosts past the cap; document it.

### MOCKINGBIRD-PERF-11: state-change bridge awaits a bus publish inline per local change
- File: src/ha.rs:236 (merge_state_changes)
- Dimension: perf. Guideline: PERF:serial-await-in-bridge
- Severity: low. Semver: none. Effort: S. Classification: n/a
- Evidence: each local `StateChanged` is forwarded to `merged` and then `bus.publish(..).await`ed in the same select loop, so announcements are capped near 1/bus-RTT; when the local broadcast lags, the skipped changes are never announced and remote nodes silently miss them (only a warning is logged). Correction: forwarding to `merged` already happens before the publish.
- Fix: hand bus events to a separate publisher task over a bounded mpsc; on overflow or `Lagged`, publish a resync/"state lagged" signal so remote nodes resync instead of missing changes.
- Risk: low; one publisher task keeps per-node order.

### MOCKINGBIRD-PERF-12: data-plane dispatch scans every mounted mock; the 404 path is O(C*H*M) under a mutex
- File: src/registry.rs:685 (dispatch :679; unmounted_host :436)
- Dimension: perf. Guideline: PERF:linear-route-scan
- Severity: low. Semver: none. Effort: M. Classification: n/a
- Evidence: per request: path `to_owned`, host String, linear scan with `strip_prefix` and `hosts.contains`, then `rewrite.apply`, a `format!` in `rewrite_path` and a MountPrefix String. A miss with a host runs `unmounted_host`, which holds `configured_hosts` and scans configured hosts times mounted mocks.
- Fix: at publish, index by first path segment plus the `/` catch-all; borrow the path; precompute the trimmed mount prefix as `Arc<str>`; precompute host to unmounted mock.
- Risk: low; keep longest-prefix and host-specific-first ordering (covered by registry tests). Only matters at hundreds of mocks.

### MOCKINGBIRD-PERF-13: serve --record holds answers twice and writes the file synchronously on a runtime worker
- File: src/snapshot.rs:210 (Answers::insert :72)
- Dimension: perf. Guideline: PERF:blocking-io-on-async
- Severity: low. Semver: none. Effort: S. Classification: n/a
- Evidence: each content String is stored in both `exact` and `operation`; dedup is a linear `list.contains`. `record()` does `writeln!` + `flush()` under a std Mutex inline on the async worker.
- Fix: store `Arc<str>` once; dedup via a per-key digest set; send lines to a dedicated writer (std thread or spawn_blocking).
- Risk: low; dev/CI feature, small next to LLM latency.

### MOCKINGBIRD-PERF-14: HA live-tail relay re-lists all mocks for each entry of an unknown mock id
- File: src/recorder.rs:259 (Remote::mock)
- Dimension: perf. Guideline: PERF:n-plus-1-reads
- Severity: low. Semver: none. Effort: S. Classification: n/a
- Evidence: a cache miss calls `store.list_all_mocks()` and misses are never cached, so each announced entry for a deleted (or not-yet-seen) mock triggers a full O(M) query while someone tails.
- Fix: look up that id only, or rate-limit full refreshes (e.g. once per second) and cache misses briefly.
- Risk: negligible.

## API and style

### MOCKINGBIRD-STYLE-1: the CLI's internals are published as a semver-bound library API
- File: src/lib.rs:1
- Dimension: api. Guideline: C-CRATE-DOC / C-HIDDEN / C-STRUCT-PRIVATE / C-STABLE
- Severity: high. Semver: breaking-needs-0.2 (doc part: none). Effort: L (doc part: S). Classification: n/a
- Evidence: lib.rs is a one-line crate doc plus 31 `pub mod`s, so clap Args enums, RuntimeDeps, App, MockRegistry and the rest are the public API of mockingbird-cli 0.1.0 on docs.rs, with no stability statement. Every internal refactor is a semver event. Merged: STYLE-15 (pre-1.0 third-party types in public signatures: axum::Router in App and `MockRegistry::router`, `axum_server::tls_rustls::RustlsConfig`, reqwest::Client/Url, PrometheusHandle, so bumping those deps is breaking) and STYLE-13 (constructors returning `Arc<Self>`; keep Arc where `self: &Arc<Self>` receivers such as `router` require it).
- Fix: now, expand the crate doc to say the library exists for the binary and its tests and carries no stability guarantee, and `#[doc(hidden)]` pure CLI plumbing (client, daemon, onboard, mcp, agents, keys, report, snapshot args). In 0.2, make those `pub(crate)`, give tests a `testing`-feature facade, and expose a deliberate surface, wrapping third-party types (e.g. a Server handle instead of a raw Router).
- Risk: doc changes are invisible to code. Narrowing visibility is breaking and must wait for 0.2; integration tests importing these modules must move behind the facade.

### MOCKINGBIRD-STYLE-2: ClientError has no Display/Error; ServeError has no source()
- File: src/client.rs:277 (ServeError at src/server.rs:25-46)
- Dimension: api. Guideline: C-GOOD-ERR / Google:non-exhaustive-enums
- Severity: medium (was high). Semver: additive (the non_exhaustive part is breaking-needs-0.2). Effort: S. Classification: n/a
- Evidence: `pub enum ClientError` derives only Debug and has From impls; no Display or Error impl exists, so it cannot be `?`-converted into anyhow/Box<dyn Error> and every caller hand-formats it. `impl std::error::Error for ServeError {}` has no `source()`; `ServeError::Other` is undocumented. Neither enum is `#[non_exhaustive]`.
- Fix: add `impl Display` (lowercase, no trailing punctuation) and `impl Error` for ClientError with `source()` for Other; give ServeError a `source()` for Other and doc the variants. `#[non_exhaustive]` in 0.2.
- Risk: if Display keeps `{e:#}` while source() is exposed, chain printers show causes twice; pick one convention.

### MOCKINGBIRD-STYLE-3: public fallible APIs use String as the error type
- File: src/file_config.rs:166 (FromStr for Role, `type Err = String`)
- Dimension: api. Guideline: C-GOOD-ERR / Google:error-reporting / Google:std-traits
- Severity: low (was medium; subsumed in practice by STYLE-1). Semver: breaking-needs-0.2. Effort: M. Classification: n/a
- Evidence: also `Result<_, String>` from tls.rs:170 check_external_url, client.rs:550 parse_trigger_spec, file_config.rs:103/611 check_bind, :355 settings, :431 validate, :444 external_host, :1399 validate_ranges, proxy/images.rs:30 ImageRule::parse and :59 validate_image_entries.
- Fix: a ConfigError enum implementing Display + Error (Send + Sync) in 0.2; `impl FromStr for ImageRule` instead of inherent `parse`. Until then, document errors.
- Risk: changing error types is breaking; callers using `.map_err(|e| anyhow!(e))` need updating.

### MOCKINGBIRD-STYLE-4: "no expect() outside tests" is not lint-enforced; expect_used allows are dead; per-module unwrap allows could be one clippy.toml
- File: Cargo.toml:127 (allows at src/agents.rs:501, daemon.rs:671, examples.rs:544, mcp.rs:2121, keys/store.rs:165, keys/page.rs:327 and 9 more unwrap-only)
- Dimension: lint. Guideline: AGENTS.md no-unwrap/expect / lint hygiene
- Severity: medium. Semver: none. Effort: S. Classification: n/a
- Evidence: `[workspace.lints.clippy]` has only `unwrap_used = "deny"`; `expect_used` stays allow-by-default, so `clippy::expect_used` in six test-module allows suppresses nothing. No clippy.toml exists. Today every `.expect(` in src sits after its `#[cfg(test)]` (maintenance.rs, ha.rs, mcp.rs), but nothing prevents a new one. Merged STYLE-5: 15 `#[allow(clippy::unwrap_used)]` on unit-test modules.
- Fix: add `expect_used = "deny"` (consider `panic`), add root clippy.toml with `allow-unwrap-in-tests = true` and `allow-expect-in-tests = true`, then delete the 15 module allows; keep (or convert to `#[expect(.., reason)]`) the inner allows in tests/*.rs.
- Risk: may surface expect() in other workspace crates and fail `-D warnings`; run clippy once the timing baseline is done.

### MOCKINGBIRD-STYLE-6: poisoned mutexes handled two ways, one of which drops writes silently
- File: src/snapshot.rs:182 (also budget.rs:121/126; registry.rs:204, 417, 437, 513, 667, 675; recorder.rs:129; watch.rs:43/50)
- Dimension: style. Guideline: Google:no-poison-internals
- Severity: low (was medium: needs a panic inside trivial critical sections). Semver: internal-only. Effort: M. Classification: n/a
- Evidence: some sites recover with `unwrap_or_else(|p| p.into_inner())`; others `if let Ok(..) = lock()` and skip. In the registry, a poisoned `configured_hosts` stops host recording and `claimed_hosts` drops configured hosts, so the proxy fails open for unmounted mocks, with no log.
- Fix: one policy: a non-poisoning mutex (parking_lot) or a helper that logs once and recovers; never drop a write without logging.
- Risk: only observable after a panic while a lock is held.

### MOCKINGBIRD-STYLE-7: about 28 public types lack Debug
- File: src/runtime.rs:66 (RuntimeDeps; also TelemetryGuard, MockRegistry, App, McpServer, ProxyState, ReloaderGuard, WatchGuard, Snapshot, StoreDataPlaneKeys and others listed in the original finding)
- Dimension: api. Guideline: C-DEBUG / C-COMMON-TRAITS
- Severity: low (was medium). Semver: additive. Effort: M. Classification: n/a
- Evidence: spot-checked 10 of the listed types: none derive or implement Debug; the only manual impls are LocalCa (proxy/ca.rs:69) and KeyPage (keys/page.rs:66).
- Fix: derive where possible; manual `finish_non_exhaustive()` impls for types holding `Arc<dyn _>`, Router or JoinHandle (pattern at keys/page.rs:66-70); consider `missing_debug_implementations = "warn"`.
- Risk: manual impls must not print secrets (tokens in McpServer, keys).

### MOCKINGBIRD-STYLE-8: no `# Errors`/`# Panics` sections and 17+ undocumented pub items; missing_docs off
- File: src/snapshot.rs:117 (also snapshot.rs:169/173/177, runtime.rs:107, budget.rs:70, registry.rs:176, ha.rs:194, and others)
- Dimension: api. Guideline: C-FAILURE / Google:panic-docs / Google:public-docs
- Severity: low (was medium; docs only). Semver: none. Effort: M. Classification: n/a
- Evidence: zero `# Errors`, `# Panics` or `# Examples` in src. Snapshot::open fails on malformed lines/version mismatch; BusStateStore::new (ha.rs:194), budget.rs:51 and maintenance spawns call tokio::spawn (panic outside a runtime). Merged STYLE-9: `dir`/`replays`/`records`, `DefaultProfile::new`, `limit`, `MockRegistry::new`, `ServeError::Other` and others have no doc comment; no `missing_docs` lint.
- Fix: add `# Errors` to entry points used by main.rs/tests first, `# Panics` to task-spawning constructors; document the bare items; then enable `missing_docs` and `clippy::missing_errors_doc`/`missing_panics_doc` once clean.
- Risk: docs only; lints under -D warnings need the backlog cleared first.

### MOCKINGBIRD-STYLE-10: bool tuples and bare bool parameters in the public API
- File: src/server.rs:60 (binds_for); src/snapshot.rs:117 (Snapshot::open); src/config.rs:452 (missing_env_api_key)
- Dimension: api. Guideline: C-CUSTOM-TYPE / Google:enum-over-bool
- Severity: low (was medium; few call sites). Semver: additive. Effort: S. Classification: n/a
- Evidence: `binds_for(role) -> (bool, bool, bool)` relies on a doc comment for position; `Snapshot::open(dir, true, false)` call sites (config.rs:316, snapshot.rs tests) are opaque.
- Fix: add `Binds { data, admin, grpc }` and `Snapshot::open_with(dir, SnapshotMode)`, deprecate the bool forms.
- Risk: deprecation warnings become errors under -D warnings; migrate tests/roles.rs (uses binds_for) in the same PR.

### MOCKINGBIRD-STYLE-11: MockRegistry accessors return anonymous String tuples
- File: src/registry.rs:390 (mounted; mounted_mocks :459, failed_mocks :471, event_issues :535)
- Dimension: api. Guideline: Google:named-fields / C-INTERMEDIATE
- Severity: low. Semver: additive. Effort: S. Classification: n/a
- Evidence: three accessors return `Vec<(String, String)>` with different meanings (workspace/name vs label/error), easy to mix up. (The claims_host part moved to PERF-2.)
- Fix: named structs (MountedMock, MockFailure) and new accessors; deprecate the tuple forms.
- Risk: additive.

### MOCKINGBIRD-STYLE-12: RAII guards that stop work on drop are not #[must_use]
- File: src/ha.rs:75 (start_with_bus returns `Arc<Self>`); types TelemetryGuard (telemetry.rs:19), WatchGuard (watch.rs:62), ReloaderGuard (tls.rs:181), Maintenance (maintenance.rs:98), LeaderWork (maintenance.rs:180)
- Dimension: api. Guideline: Google:must-use
- Severity: low. Semver: additive. Effort: S. Classification: n/a
- Evidence: no `#[must_use]` in the crate. `watch::start(..)?;` discards the WatchGuard and stops watching at once. Note: `#[must_use]` on `Ha` does not propagate through `Arc<Ha>`, so `start_with_bus` itself needs the attribute.
- Fix: `#[must_use = "dropping the guard stops ..."]` on the guard types and on fns returning `Arc<Ha>` directly.
- Risk: fails -D warnings builds of any in-repo caller that discards a guard (the intent); fix those sites together.

### MOCKINGBIRD-STYLE-14: literal-constructible public structs everywhere, and a pub JoinHandle inside a guard
- File: src/tls.rs:181 (ReloaderGuard); also RuntimeDeps runtime.rs:66, App config.rs:237, Listeners server.rs:50
- Dimension: api. Guideline: C-STRUCT-PRIVATE / Google:non-exhaustive-structs
- Severity: low. Semver: breaking-needs-0.2. Effort: M. Classification: n/a
- Evidence: hundreds of pub fields; the only `non_exhaustive` text in src is a Debug helper call. `pub struct ReloaderGuard(pub JoinHandle<()>)` (built at config.rs:922) exposes the handle its Drop aborts.
- Fix: in 0.2, private ReloaderGuard field with `new`, `#[non_exhaustive]` on growing non-clap structs with builders for tests (tests/chaos.rs, protocols_e2e.rs build ServeArgs literally). Fold into the STYLE-1 narrowing.
- Risk: breaking; tests using struct literals must move to builders.

### MOCKINGBIRD-STYLE-16: non-leaf modules define API items
- File: src/proxy/mod.rs:47 (HostClaims :30, ProxyState :47); src/keys/mod.rs:16-81
- Dimension: style. Guideline: Google:no-api-in-nonleaf
- Severity: low. Semver: none. Effort: S. Classification: n/a
- Evidence: proxy/mod.rs declares submodules and also defines HostClaims, ProxyState and consts; keys/mod.rs defines KeysCommand, SetupArgs, page_options and setup next to `pub mod page; pub mod store;`.
- Fix: move to proxy/state.rs and keys/setup.rs with `pub use` re-exports so every path still resolves.
- Risk: pure move with re-exports; no API change.

## Stubs and unfinished behaviour

### MOCKINGBIRD-STUB-1: vault: secret references are rejected for LLM api_key, header refs and broker SASL passwords
- File: src/reconcile.rs:802 (valid_secret_ref; callers reconcile.rs:286, :298, file_config.rs:953)
- Dimension: stub. Guideline: StyleGuide:docs-match-behaviour
- Severity: high. Semver: none (pub(crate) fn; widens accepted input). Effort: S. Classification: finish
- Evidence: `valid_secret_ref` accepts only `env:` and `file:`. docs/configuration.md:9 says any secret-taking key accepts `vault:<mount>/<path>#<field>`; docs/deploy/kubernetes.md:171 advertises vault for Admin-API profiles. mb-store's `is_secret_reference` (crates/mb-store/src/model.rs:214) already accepts `vault:`, and mb-ext has a vault resolver (crates/mb-ext/src/vault.rs). Error texts say "(env:NAME or file:/path)".
- Fix: accept `vault:` (non-empty, no whitespace), ideally by reusing mb-store's check; update the three messages; apply the same to mb-admin `check_secret_ref` (outside this group); add tests for a profile api_key and a SASL password with `vault:`.
- Risk: a vault ref with VAULT_ADDR unset then fails at resolve time instead of load; consider a load-time warning.

### MOCKINGBIRD-STUB-2: `mockingbird run --replay/--record/--offline/-c` are ignored without warning when a server already answers
- File: src/run.rs:186 (prepare; the flags are only used in the StartArgs at :150-168)
- Dimension: stub. Guideline: C-GOOD-ERR
- Severity: low (was medium: the flag help says "for a server `run` starts", run.rs:31-41, so it is documented, but nothing is printed at runtime). Semver: none. Effort: S. Classification: finish
- Evidence: when `answers(admin_url)` is true, prepare goes straight to `wait_ready` and runs the command against the existing server; a leftover dev server turns `run --replay snap -- npm test` into live, paid LLM calls.
- Fix: when any of these flags was given and a server already answers, print a stderr warning (or fail behind a flag), ideally comparing with the running server's mode via readyz/doctor.
- Risk: failing hard could break scripts that reuse a server on purpose; start with a warning.

### MOCKINGBIRD-STUB-3: snapshot export/import drops path_rewrite and header_overrides
- File: src/snapshot.rs:569 (MOCK_FIELDS)
- Dimension: stub. Guideline: StyleGuide:no-silent-data-loss
- Severity: medium. Semver: none (snapshot file format; bump FORMAT_VERSION if the layout changes). Effort: M. Classification: finish
- Evidence: MOCK_FIELDS copies name, route_prefix, persona, consistency, unknown_ids and hosts. The doc comment at :567-568 deliberately excludes LLM profile, scenarios and auth, but path_rewrite and header_overrides (crates/mb-admin/src/mocks.rs:224, :249) are dropped without mention, so an imported mock that keeps its hosts routes differently. docs/ci.md:88 says the files carry "the mocks and their active contracts" without the limits.
- Fix: add path_rewrite and header_overrides to MOCK_FIELDS; decide (ask-user) whether scenarios/default_scenario, chaos, limits, webhooks and data_plane_auth belong in a snapshot; document exactly what a snapshot carries in docs/ci.md.
- Risk: webhook targets and auth are environment-specific; keep them opt-in. Import must stay tolerant of v1 snapshots.

### MOCKINGBIRD-STUB-4: `hosts` on a gRPC mock has no effect, yet the proxy intercepts those hosts into the HTTP data plane
- File: src/registry.rs:104 (Mounted::hosts; configured_hosts filled at :204-210; proxy upstream config.rs:757-763)
- Dimension: stub. Guideline: StyleGuide:no-silent-fallback
- Severity: low (was medium; edge case). Semver: none. Effort: M. Classification: ask-user
- Evidence: a gRPC mock's `hosts()` is empty, but reload_all puts every stored mock's hosts into `configured_hosts`, which `claimed_hosts` returns; the proxy serves claimed hosts with the HTTP `data` router, so a gRPC client through the proxy gets intercepted and a 404/503, never passed through or served.
- Fix: product decision: (a) reject or warn on `hosts` for gRPC contracts and document that the proxy does not intercept gRPC, or (b) route `application/grpc` on intercepted hosts to the GrpcServer. Until (b), leave gRPC mocks' hosts out of configured_hosts.
- Risk: (a) rejects configs that load today; warn first.

### MOCKINGBIRD-STUB-5: the CLI cannot finish its own import workflow
- File: src/client.rs:829 ("review it in the console")
- Dimension: stub. Guideline: StyleGuide:feature-parity
- Severity: medium. Semver: additive (new CLI surface). Effort: M. Classification: ask-user
- Evidence: ClientCommand has Mocks{List,Get,Create,Delete}, Import, Sessions, ExportFixtures, SeedFixtures, Channels, Scenarios, Profiles, Brokers, Webhooks, Sign; no mocks update, no contract version list/activate, no draft approve, no request-log listing. `import --approve` on a draft with warnings stops at "review it in the console". MCP covers these with update_mock, approve_draft and the generic admin_api tool (mcp.rs:443, :534, :734).
- Fix: add `mockingbird api <METHOD> <PATH> [--body FILE]` reusing `client::api_path` (S); optionally `contracts list|activate|approve` and `mocks update --patch FILE`; make the message at :829 name the follow-up command.
- Risk: additive; keep names consistent with the MCP tools.

### MOCKINGBIRD-STUB-6: on Windows, `server stop` always force-kills and reports `forced: false`
- File: src/daemon.rs:400 (terminate; stop at :422; server.rs:297 pending term future)
- Dimension: stub. Guideline: C-GOOD-ERR
- Severity: low. Semver: none. Effort: M. Classification: finish
- Evidence: the non-unix `terminate(pid, _force)` always runs `taskkill /T /F`; serve's Windows termination future is `pending()` and a DETACHED_PROCESS child has no console for Ctrl-C. `stop` reports `forced: false` after a hard kill; in-flight requests, webhook deliveries and unflushed budget spend are lost. `stop_child` (used by run) does the same.
- Fix: add a console-free graceful path (loopback-only authenticated shutdown endpoint, stop-file watcher, or CREATE_NEW_PROCESS_GROUP plus CTRL_BREAK with `tokio::signal::windows::ctrl_break`); use `/F` only when forcing; report `forced` accurately.
- Risk: a shutdown endpoint must be loopback and auth only; Windows-only code not covered by unix CI.

### MOCKINGBIRD-STUB-7: MCP advertises `logging`, but setLevel is a no-op and no log notifications are sent
- File: src/mcp.rs:1804 (capability at :1824)
- Dimension: stub. Guideline: MCP:capability-honesty
- Severity: low. Semver: none. Effort: S. Classification: finish
- Evidence: `"logging": {}` is advertised; `logging/setLevel` returns `{}` without storing anything; no `notifications/message` emitter exists in mcp.rs.
- Fix: drop `logging` from capabilities (keep the handler returning `{}`), or store the level and forward tracing events and import progress as `notifications/message`.
- Risk: low.

### MOCKINGBIRD-STUB-8: the MCP `admin_api` escape hatch fails on non-JSON responses
- File: src/client.rs:1135 (AdminClient::request JSON parse; admin_api at mcp.rs:1458)
- Dimension: stub. Guideline: C-GOOD-ERR
- Severity: low. Semver: none. Effort: S. Classification: finish
- Evidence: every successful body is parsed as JSON, else "the Admin API sent a non-JSON ... response", so `GET .../contracts/{v}/source` fails for any non-JSON contract. For an SSE path, `resp.text()` waits for the stream to end, so the call hangs rather than errors.
- Fix: branch on Content-Type in `api_path`: `{content_type, text}` for UTF-8, `{content_type, base64, bytes}` for binary (size-capped); refuse `text/event-stream` up front, pointing to list_requests.
- Risk: low; JSON behaviour unchanged.

### MOCKINGBIRD-STUB-9: --watch warns only for proxy:/brokers: edits; other read-at-startup sections change silently
- File: src/config.rs:863 (prepare hook :857-871; applied_hook :1121)
- Dimension: stub. Guideline: StyleGuide:no-silent-fallback
- Severity: low. Semver: none. Effort: S. Classification: finish
- Evidence: prepare warns when `cfg.proxy != initial_proxy` and rejects broker changes; `applied_hook` applies only request-log settings and llm.default_profile. Edits to server, storage, auth, limits, protocols, pii, import, outbound, audit or bus are ignored with no log, contrary to the comment's own rule ("say so instead of ignoring an edit silently").
- Fix: snapshot those sections next to `initial_proxy` and warn once per changed section ("config reload: <section> changed; restart to apply it").
- Risk: compare after apply_env/apply_flags (as prepare already does) to avoid false warnings.

### MOCKINGBIRD-STUB-10: OTLP exporter forced to HTTP/protobuf; grpc protocol env silently overridden; traces-only endpoint not an enable signal
- File: src/telemetry.rs:93 (init at :44)
- Dimension: stub. Guideline: StyleGuide:no-silent-fallback
- Severity: low. Semver: none. Effort: S. Classification: keep-refusal
- Evidence: `.with_http().with_protocol(Protocol::HttpBinary)` ignores `OTEL_EXPORTER_OTLP_PROTOCOL=grpc`; export is enabled only by `OTEL_EXPORTER_OTLP_ENDPOINT`, not `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT`. HTTP-only is deliberate and documented.
- Fix: keep HTTP; warn when the protocol env says grpc; treat the traces endpoint as an enable signal; note both in docs/configuration.md.
- Risk: minimal.

### MOCKINGBIRD-STUB-11: passthrough_unsupported message is wrong for Claude Code --scope user
- File: src/agents.rs:466 (env_syntax at :155)
- Dimension: stub. Guideline: C-GOOD-ERR
- Severity: low. Semver: none. Effort: S. Classification: keep-refusal
- Evidence: the message always says "<client> cannot read variables in its MCP config"; for Claude Code user scope the real reason (comment at :153-154) is that the printed `claude mcp add` line would be shell-expanded into ~/.claude.json, and project scope does expand `${NAME:-}`.
- Fix: keep the refusal; branch the message for Claude Code user scope and suggest `--scope project` or `mockingbird keys setup`.
- Risk: wording only; update any test asserting the string.

### MOCKINGBIRD-STUB-13: stale known-limit says the console Channels tab "comes in P6"
- File: docs/events.md:156
- Dimension: stub. Guideline: StyleGuide:docs-match-behaviour
- Severity: low. Semver: none. Effort: S. Classification: finish
- Evidence: crates/mb-console/web/src/pages/mocks/ already has Channels.tsx, PublishDialog.tsx and ChannelTriggersEditor.tsx.
- Fix: delete the bullet or point to the console Channels tab.
- Risk: none.

## Tests

### MOCKINGBIRD-TESTS-1: fixed 1.5 s negative waits for rejected reloads can wait on readyz instead
- File: tests/config.rs:1188 (also :1228, :1237)
- Dimension: tests. Guideline: PERF:fixed-sleep-negative-wait
- Severity: low (was medium). Semver: none. Effort: S. Classification: n/a
- Evidence: invalid_reload_keeps_previous_state and reload_changing_brokers_is_rejected sleep 1500 ms after each bad write. A failed reload (parse or prepare/broker check) reaches `status.failed` (src/watch.rs:254) and /readyz reports `config_reload: <error>` (src/ops.rs:127); `wait_for_readyz` exists at tests/config.rs:3209.
- Fix: after each bad write, `wait_for_readyz(&app.admin, |b| reason contains config_reload)`; for the second broker edit, wait for the reason text to change to the client_id_prefix error; then assert previous state.
- Risk: low; assertions get stronger and ~3 s is saved.

### MOCKINGBIRD-TESTS-2: "let the consumer subscribe" sleeps can poll MemoryBroker::subscribed_groups
- File: tests/events_e2e.rs:124 (also tests/events_workspaces.rs:473 in twins())
- Dimension: tests. Guideline: PERF:event-driven-wait
- Severity: low (was medium). Semver: none. Effort: S. Classification: n/a
- Evidence: `inject()` delivers only to current subscribers (crates/mb-protocols/src/events/memory.rs:92); a late subscription loses the message. `subscribed_groups()` (:97) records each subscription.
- Fix: bounded poll until `subscribed_groups().len() >= N` (2 s cap) instead of both sleeps.
- Risk: low; removes a load-dependent race.

### MOCKINGBIRD-TESTS-3: unsafe std::env::set_var in parallel tests
- File: tests/webhooks_e2e.rs:143 (also :195)
- Dimension: tests. Guideline: Rust2024 unsafe set_var
- Severity: medium. Semver: none. Effort: S. Classification: n/a
- Evidence: two tests call `unsafe { std::env::set_var(SECRET_VAR, ..) }` while other tests in the binary run on other threads (including resolver threads reading environ via libc); the SAFETY comment does not hold. It also blocks merging test binaries (TESTS-5).
- Fix: write the secret to a tempdir file and pass `file:<path>` (supported in crates/mb-ext/src/secrets.rs:72); drop set_var and SECRET_VAR.
- Risk: very low; env: resolution stays covered elsewhere.

### MOCKINGBIRD-TESTS-4: otlp_disabled_without_endpoint installs a process-global tracing subscriber
- File: tests/observability.rs:1073
- Dimension: tests. Guideline: tests:global-state
- Severity: medium. Semver: internal-only. Effort: S. Classification: n/a
- Evidence: `telemetry::init_with(.., None).unwrap()` calls `try_init()` (src/telemetry.rs:77), installing an info-level fmt layer writing to `io::stderr()` directly, which libtest does not capture, so later tests in the binary spill logs; in a merged binary a second init panics.
- Fix: factor the endpoint decision into a small pure helper (pub(crate) or doc(hidden)) and unit-test it in src/telemetry.rs; remove the global init from tests/.
- Risk: low; keep `init_with`'s public signature.

### MOCKINGBIRD-TESTS-5: 31 integration-test binaries; merge into tests/it with a common module
- File: crates/mockingbird/Cargo.toml:100 (dev-dependencies; tests/ has 31 files)
- Dimension: tests. Guideline: tests:binary-consolidation
- Severity: medium. Semver: none. Effort: L. Classification: n/a
- Evidence: 31 standalone test crates each link the full tree; .github/workflows/ci.yml:61-63 already disables test debuginfo for disk. Helpers (fixture, serve, start, free_port, ...) are copied across files; no `mod common`, `[[test]]` or harness entries.
- Fix: tests/it/main.rs with `mod common;` and per-file modules; fix TESTS-3, TESTS-4, TESTS-8 and TESTS-9 first; keep separate serial guards (ha_cluster::SERIAL, roles::PG_SERIAL); update docs naming tests/docs_reference.rs and `--test <name>` references.
- Risk: medium; more in-process parallelism exposes timing-sensitive tests. Run the suite several times.

### MOCKINGBIRD-TESTS-6: swap_during_inflight_requests cannot fail
- File: tests/registry.rs:362
- Dimension: tests. Guideline: tests:weak-assertion
- Severity: medium. Semver: none. Effort: S. Classification: n/a
- Evidence: 20 requests with 200 ms latency, a 50 ms sleep, then the swap; v1 and v2 both answer 200 at /orders/orders, so the asserts pass whether or not requests were in flight or rerouted. Only /orders/ping afterwards proves the swap.
- Fix: assert /orders/ping is 404 before the swap; record `swapped = Instant::now()` after reload_mock and have tasks return completion instants, asserting each completes after `swapped`; better, wait for in-flight via the request log or a gauge.
- Risk: low.

### MOCKINGBIRD-TESTS-7: sleep-then-assert on readyz leader flag; oversized leader-loss negative wait
- File: tests/roles.rs:306 (also :258, :275, :281)
- Dimension: tests. Guideline: PERF:poll-until-helper
- Severity: low. Semver: none. Effort: S. Classification: n/a
- Evidence: sends leader=true, sleeps 100 ms, reads /readyz once; the negative wait at :281 is 1500 ms against a 500 ms debounce (src/watch.rs:22, `pub const DEBOUNCE`).
- Fix: poll readyz until `leader == true` (2 s cap); use `mockingbird::watch::DEBOUNCE * 2` at :281.
- Risk: low.

### MOCKINGBIRD-TESTS-8: wall-clock perf asserts over heavy fixtures
- File: tests/observability.rs:763 (fixture at :733-750; also :871)
- Dimension: tests. Guideline: tests:slow-test
- Severity: low. Semver: none. Effort: S. Classification: n/a
- Evidence: builds a 100,000-object array plus a 5 MiB string three times, then the writer redacts and serializes each; asserts fastest `record()` < 50 ms. recorder_drops_when_full_without_blocking asserts 5000 records < 1 s.
- Fix: shrink to ~10k objects / 1 MiB (truncation and redaction still apply), or assert structurally using `stub.stall_append`; keep budgets loose.
- Risk: low.

### MOCKINGBIRD-TESTS-9: free_port() TOCTOU, with child stderr discarded
- File: tests/e2e.rs:207 (copies in ci.rs:18, run.rs:14, agent_toolkit.rs:459; 20 call sites)
- Dimension: tests. Guideline: tests:fixed-port
- Severity: low. Semver: none. Effort: M. Classification: n/a
- Evidence: binds :0, drops the listener, and a spawned `mockingbird serve` binds later; stderr is `Stdio::null()` (e2e.rs:236, :458, :507), so a lost race shows only as "server did not start".
- Fix: one shared helper with retry on address-in-use, or `--port 0` plus a reported bound address; capture child stderr into the panic message.
- Risk: low.

### MOCKINGBIRD-TESTS-10: fixed 100 ms sleep before reading ca.pem written by a spawned task
- File: tests/proxy.rs:442
- Dimension: tests. Guideline: PERF:poll-until-helper
- Severity: low. Semver: none. Effort: S. Classification: n/a
- Evidence: spawns run_standalone, sleeps 100 ms, then `std::fs::read(ca.pem).unwrap()`; slow CA generation panics with ENOENT.
- Fix: poll for a non-empty file (5 s cap), or create the CA with `LocalCa::load_or_create` before spawning.
- Risk: very low.

### MOCKINGBIRD-TESTS-11: live-feed tests sleep instead of confirming the subscription
- File: tests/ha.rs:262 (also tests/ha_cluster.rs:346; negative waits ha.rs:243, events_workspaces.rs:644/654)
- Dimension: tests. Guideline: PERF:event-driven-wait
- Severity: low. Semver: none. Effort: M. Classification: n/a
- Evidence: opens the stream, sleeps 100/200 ms, posts once, waits for the marker; a late server-side subscribe loses the event.
- Fix: re-post with fresh markers every ~100 ms until one appears; for negative waits, first wait on a positive event proving the other side processed the change.
- Risk: low.

### MOCKINGBIRD-TESTS-12: python urllib3 preload test silently skips when urllib3/certifi are missing
- File: tests/proxy.rs:643 (helper :626)
- Dimension: tests. Guideline: tests:silent-skip
- Severity: low. Semver: none. Effort: S. Classification: n/a. Verdict: PLAUSIBLE
- Evidence: returns early with "skipped:" when `python3 -c "import urllib3, certifi"` fails; ci.yml has no pip step, so whether it runs depends on the runner image.
- Fix: `pip install urllib3 certifi` in the CI test job (or set MB_TEST_PYTHON) and panic instead of skipping when `CI` is set.
- Risk: very low.
