# Mockingbird P7 Enterprise Hardening and Readiness Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make Mockingbird deployable as a highly available, hardened, signed and documented platform: split `control`/`data` roles over a cross-node bus with leader election, a Redis state backend, per-mock chaos and limits, an output PII scrubber, a Helm chart, a signed release pipeline, nightly LLM evals, `mb-ext` 1.0, the deferred console/CLI work, and the readiness documents.

**Architecture:**
- **`mb-state`** gains `redis.rs` (a `StateStore` over Redis), `bus.rs` (a cross-node `Bus`: in-process, Postgres `LISTEN/NOTIFY`, Redis pub/sub) and `leader.rs` (`Leadership`: always / Postgres advisory lock / Redis lease). Bus payloads carry ids only. Receivers reload from the stores.
- **`mb-engine`** gains `pii.rs` (the `Scrubber`) and `guards.rs` (`EngineGuards`: a global generation semaphore, a per-mock `TokenBudget`, the scrubber and a response-size cap). The guards wrap every LLM call. When a guard refuses, the call falls back to the faker and never errors.
- **`mb-protocols`** gains `limits.rs` (a per-mock token bucket and request-size caps) and `chaos.rs` (error injection, latency jitter, stream drops). It also takes the P4 review carry-overs: GraphQL root-field and subscription caps, per-item metering of client/bidi gRPC streams, rejection logging, and `?wsdl` behind a proxy.
- **`mb-store`** gains migration `0300_hardening`: `mocks.chaos`, `mocks.limits`, a `token_usage` table and a request-log lookup by id.
- **`mockingbird`** gains:
  - `server.rs`: `serve` moves out of `main.rs` so other binaries can reuse it.
  - `--role all|control|data`.
  - `ha.rs`: bridges store, state and request-log changes onto the bus.
  - leader-gated reconcile, watch and maintenance.
  - `budget.rs`: a store-backed daily token budget.
  - TLS termination and outbound CA/proxy settings.
  - the new config sections.
  - `--workspace` and the `scenarios`/`profiles` subcommands in the CLI.
- **`mb-ext` 1.0**: a frozen, fully documented API, `#[non_exhaustive]` where the core constructs values, `vault:` secret references, a worked extension example and a semver gate.
- **`mb-console`**: a Channels tab and a chaos/limits editor.
- **New crate `mb-evals`** (`publish = false`): nightly evals against a real provider, plus an offline throughput bench.
- **Delivery and documents:**
  - `deploy/helm/mockingbird`, and Compose profiles for HA and brokers;
  - `release.yml` (multi-arch images, cosign, CycloneDX SBOM, SLSA provenance, binaries, the `mb-ext` crate) and `evals.yml`;
  - community files and operator docs.

**Tech Stack:** P1–P6 stack plus these exact pins in `[workspace.dependencies]`:
- `redis = { version = "1.7.1", default-features = false, features = ["tokio-comp", "connection-manager", "tokio-rustls-comp", "script"] }`
- `regex = "1.13"` (already in the lockfile transitively; used by `mb-engine`)
- `axum-server = { version = "0.8.0", default-features = false, features = ["tls-rustls-no-provider"] }`
- `rustls = { version = "0.23", default-features = false, features = ["std", "aws_lc_rs"] }` (the provider `jsonwebtoken` already pulls in)
- CI-only tools, pinned in workflows:
  - `cargo-zigbuild 0.20` (musl cross builds);
  - `cargo-cyclonedx 0.5.9`;
  - `cargo-semver-checks` (latest release action `obi1kenobi/cargo-semver-checks-action@v2`);
  - helm `v3.19.0`, kubeconform `v0.7.0`;
  - `sigstore/cosign-installer@v3`, `aquasecurity/trivy-action@0.33.1`;
  - `actions/attest-build-provenance@v3`, `actions/attest-sbom@v3`.

**Spec:** `docs/superpowers/specs/2026-10-14-mockingbird-platform-design.md`. This plan covers the P7 row of §16 and implements:
- §3.2 (`mb-ext` 1.0 and the default `SecretStore`);
- §4.3 (roles);
- §7.1 (chaos);
- §8.2–8.3 (Redis state, buses);
- §11.3 (PII scrubber, limits, TLS, CA bundles, proxy);
- §12 (Helm, Compose profiles, distribution, supply chain, leader election);
- §14 (per-stage spans, state-size metric);
- §15 (nightly evals, image scan, Compose e2e flow).

The controller's `p7-decisions.md` is binding. All of its items, including the P4 carry-overs, the Channels tab and `--workspace`, are restated in the tasks below.

## Global Constraints

### Baseline and parallel work
- P7 branches from `main` **after P5 (events) and P6 (smart import) are merged**. Tasks 9, 14 and 18 edit files P6 also edits:
  - `crates/mockingbird/src/{client.rs,runtime.rs,config.rs}`;
  - console `MockLayout.tsx` and `router.tsx`.

  P6's names (`build_engine`, `import_provider`, `ImportArgs.source`, the `Import` tab, `App.import_workers`) are used as they are.
- Every P1–P6 Global Constraint still holds:
  - no `unwrap()`/`expect()` outside tests;
  - `cargo fmt --all --check` and `cargo clippy --workspace --all-targets --all-features -- -D warnings` must pass;
  - Admin API errors are RFC 9457 problems;
  - `DEFAULT_WORKSPACE = "default"`;
  - secrets are only `env:`/`file:` references (plus `vault:` after Task 2);
  - Postgres tests are gated by `MB_TEST_POSTGRES_URL` and run with `scripts/test-postgres.sh`;
  - every new serde field on a stored type is `#[serde(default, skip_serializing_if = …)]`, so stored P1–P6 JSON and `insta` snapshots stay byte-identical;
  - `mb-engine`, `mb-store` and `mb-state` stay axum-free; `mb-protocols` depends on neither `mb-store` nor `mb-auth`;
  - `npm run check:api` must pass after any Admin API change.

### Migrations and store names
- New files are `crates/mb-store/migrations/{sqlite,postgres}/0300_hardening.sql`. Never reuse the P5/P6 names:
  - tables `channel_settings`, `mock_event_settings`, `scheduler_leases`, `import_jobs`;
  - columns `request_log.kind` and `contract_versions.draft_meta`.
- P7 adds columns `mocks.chaos` (nullable JSON) and `mocks.limits` (JSON, default `'{}'`), and the table `token_usage(mock_id, day, tokens)`.

### Build and parallel execution
- Use `CARGO_INCREMENTAL=0 CARGO_TARGET_DIR=/home/user/mb-p7-target`, **shared by all concurrent worktrees**. Cargo's build lock serializes the builds, and the disk has about 15 GB free.
- At most 2 Rust tasks run at once. Console-only, docs-only and workflow-only tasks may run alongside them.
- Docker is unavailable locally:
  - Image, Compose, Helm-in-cluster and release jobs are verified in CI only.
  - Each such task still has a local gate: a Rust test that parses its YAML, or `scripts/check-helm.sh`, which downloads helm and kubeconform into `.tools/` when the network allows and otherwise prints `skipped: …`.
- Redis tests read `MB_TEST_REDIS_URL` and return early with `eprintln!("skipped: …")` when it is unset.
  - `scripts/test-redis.sh [cargo test args…]` starts the local `/usr/bin/redis-server` on a free port, exports the variable and runs `cargo test --workspace --all-features "$@"`.
  - `MB_TEST_POSTGRES_URL` and `MB_TEST_REDIS_URL` may be combined: run `scripts/test-postgres.sh` inside `scripts/test-redis.sh`.
- **Shared files have one owner each**, so parallel tasks do not collide:
  - `.github/workflows/ci.yml`: Task 23 only. New workflows are new files.
  - `README.md`: Task 24 only.
  - `crates/mockingbird/src/file_config.rs`: Task 10 defines every new key. Later tasks only read them.
  - `crates/mb-console/web/src/api/schema.d.ts` is generated. On a merge conflict, run `npm run gen:api` again; never hand-merge it.

### Exact values (config key → default → valid range)

| Key | Default | Range / rule |
|---|---|---|
| `server.role` (`--role`, `MB_ROLE`) | `all` | `all`, `control`, `data`. `control`/`data` need a `postgres://` `storage.url`, a `postgres://` or `redis(s)://` effective state URL, and are refused with SQLite or `memory`. |
| `server.trust_forwarded_headers` | `false` | — |
| `server.tls.cert_file` / `key_file` | unset | Both or neither. Files must be readable PEM at startup. |
| `outbound.ca_file` | unset | PEM bundle, added to the default roots |
| `outbound.proxy` / `outbound.no_proxy` | unset / `[]` | `http(s)://` URL; host or domain-suffix list |
| `bus.url` (`MB_BUS_URL`) | unset | Unset: Postgres `LISTEN/NOTIFY` when `storage.url` is Postgres, else in-process. Otherwise `redis://`, `rediss://`, `postgres://` or `local`. |
| `storage.state_url` (`MB_STATE_URL`) | unset (existing key) | adds `redis://` / `rediss://` |
| `limits.max_concurrent_generations` | 64 | 1..=4096 |
| `limits.max_request_bytes` | 1 048 576 | 1024..=67 108 864 |
| `limits.max_response_bytes` | 1 048 576 | 1024..=16 777 216 |
| `mocks.<m>.limits.rate_limit.{requests_per_second,burst}` | unset | rps in (0, 100 000]; burst 1..=100 000 |
| `mocks.<m>.limits.daily_token_budget` | unset | ≥ 1 (tokens per UTC day, all replicas) |
| `mocks.<m>.limits.max_request_bytes` / `max_response_bytes` | global value | same ranges as global |
| `mocks.<m>.chaos.error_rate`, `drop_stream_rate` | 0 | 0.0..=1.0 |
| `mocks.<m>.chaos.latency_jitter.{min,max}` | unset | durations; min ≤ max ≤ 30 s |
| `mocks.<m>.scenarios.<s>.chaos` | unset | replaces the mock's chaos while the scenario applies |
| `pii.enabled` | `false` | — |
| `pii.patterns` | all six builtins | subset of `email`, `phone`, `credit_card`, `us_ssn`, `iban`, `ipv4` |
| `pii.custom[]` `{name, regex, replacement}` | `[]` | ≤ 32 entries; name `[a-z0-9_]{1,32}`; regex compiled with size limit 1 MiB; replacement template: `#` → random digit, `?` → random lowercase letter, anything else literal |
| `protocols.graphql.max_root_fields` | 20 | 1..=1000 |
| `protocols.graphql.max_subscriptions_per_connection` | 16 | 1..=1000 |
| `protocols.graphql.max_connections_per_mock` | 1000 | 1..=100 000 |
| `protocols.sse.keepalive` | (existing) | ≥ 1 s |
| `protocols.graphql.path` | (existing) | starts with `/` |
| `protocols.grpc.max_message_bytes` | (existing) | ≥ 1024 |
| `protocols.stream.item_interval`, scenario `stream_item_interval` | (existing) | ≤ 30 s (file config and Admin API) |

### Wire strings and constants
- Request header `X-Mockingbird-Chaos: off` bypasses chaos, honoured only when header overrides are enabled. A response with an injected error carries `X-Mockingbird-Chaos: error`; gRPC carries the metadata `x-mockingbird-chaos: error`, and a GraphQL root field carries `extensions.chaos = "error"`.
- **Bus:**
  - Postgres channel `mockingbird_bus`, Redis channel `mockingbird:bus`.
  - Payload: JSON `BusEnvelope { origin, event }`, at most `MAX_BUS_PAYLOAD = 7900` bytes. A larger event is sent as `Resync`.
  - Reconnect backoff: 1 s, doubling to 30 s.
- **Leader:**
  - Lock name `control`. Postgres advisory key = the first 8 bytes (big-endian `i64`) of sha256(`"mockingbird:control"`).
  - Redis key `mockingbird:leader:control`.
  - `LEADER_LEASE_TTL` = 15 s, `LEADER_RENEW_INTERVAL` = 5 s.
- `BUDGET_FLUSH_INTERVAL` = 5 s.
- **Metrics (exact names):**

| Metric | Type | Labels |
|---|---|---|
| `mb_generation_in_flight` | gauge | — |
| `mb_generation_queue_timeouts_total` | counter | — |
| `mb_token_budget_exhausted_total` | counter | `mock` |
| `mb_token_budget_used` | gauge | `mock` |
| `mb_rate_limited_total` | counter | `mock` |
| `mb_pii_replacements_total` | counter | `pattern` |
| `mb_chaos_injected_total` | counter | `mock`, `kind` = `error`/`latency`/`drop` |
| `mb_bus_connected` | gauge | — |
| `mb_bus_events_total` | counter | `direction` = `sent`/`received`, `type` |
| `mb_leader` | gauge | — |
| `mb_state_sessions` | gauge | `mock` |

- Span names (Task 22): `mb.decode`, `mb.admission`, `mb.state`, `mb.prompt`, `mb.llm`, `mb.validate`, `mb.repair`, `mb.encode`.

## Review Focus

1. **The bus drops or misses notifications** (Postgres restart, Redis failover, NOTIFY payload too large).
   - Expected: on reconnect the bus emits `Resync` and every receiver reloads every mock. `/readyz` reports `degraded` with `bus: <error>` while the bus is disconnected. No data node keeps serving a deleted mock for more than one reconnect cycle.
   - Tests: Task 11 `pg_bus_reconnect_emits_resync`, `redis_bus_reconnect_emits_resync`, `oversized_event_becomes_resync`; Task 15 `resync_reloads_all_mocks`, `bus_down_degrades_readyz`.
2. **Leadership is lost or contested** (database connection drop, two control nodes starting together).
   - Expected: at most one leader at any time. A node that loses its connection stops leader-only work (reconcile, watch, maintenance) within `LEADER_RENEW_INTERVAL`, and another node takes over within `LEADER_LEASE_TTL`.
   - Tests: Task 11 `pg_leader_single_holder`, `pg_leader_lost_on_connection_drop`, `redis_leader_expires_and_fails_over`; Task 18 `only_leader_reconciles`, `leader_gain_starts_watch`.
3. **The PII scrubber hits false positives or breaks the contract.**
   - Expected: UUIDs, ISO dates, order numbers, version strings and Luhn-invalid digit runs are untouched. Scrubbed output still validates against the response schema. Scrubbing already-scrubbed output changes nothing.
   - Tests: Task 3 `leaves_uuids_dates_and_order_numbers`, `luhn_invalid_digits_untouched`, `scrub_is_idempotent`; Task 12 `scrubbed_output_still_validates`.
4. **Limits under load.**
   - Expected:
     - A saturated generation semaphore falls back to the faker within the call timeout; it never hangs.
     - A rate-limited call answers 429 with `Retry-After`, using the contract's 429 shape where one is declared.
     - Once the daily budget is exhausted, calls are served by the faker and `/readyz` says so.
     - An over-cap request is 413 and is recorded.
   - Tests: Task 12 `saturated_limiter_falls_back_within_timeout`; Task 14 `rate_limited_call_is_429_with_retry_after`, `budget_exhausted_serves_fallback_and_degrades_readyz`, `oversized_request_is_413_and_recorded`.
5. **Chaos escapes the contract or ignores the user's switch.**
   - Expected: injected errors use the operation's declared error shape (or the protocol's native 503/UNAVAILABLE/Fault when none is declared). A rate of `0` never injects. `X-Mockingbird-Chaos: off` bypasses chaos only when header overrides are enabled. A scenario's chaos replaces the mock's.
   - Tests: Task 19 `error_uses_declared_variant`, `undeclared_error_is_protocol_native`, `zero_rate_never_injects`, `header_off_bypasses_only_when_overrides_enabled`, `scenario_chaos_replaces_mock_chaos`.

---

## Waves and dependencies

A task starts when every task it depends on is merged. Tasks in the same wave are independent of each other and may run in parallel worktrees (subject to the two-Rust-tasks rule).

| Wave | Task | Depends on | Kind |
|---|---|---|---|
| 1 | 1 Store: chaos, limits, token usage, request-log lookup + Admin API | — | Rust + `gen:api` |
| 1 | 2 `mb-ext` 1.0 API freeze and `vault:` secrets | — | Rust |
| 1 | 3 PII scrubber module | — | Rust |
| 1 | 4 Redis state store | — | Rust |
| 1 | 5 Community and repository files, example gallery | — | docs + Rust test |
| 1 | 6 Release pipeline | — | workflow + Rust test |
| 1 | 7 `mb-evals`: nightly evals and throughput bench | — | Rust + workflow |
| 1 | 8 Console Channels tab | — | console (+ one Admin API query param) |
| 1 | 9 CLI `--workspace`, `scenarios`, `profiles` | — | Rust |
| 2 | 10 Config surface and validation | 1 | Rust |
| 2 | 11 Bus and leader election (`mb-state`) | 4 | Rust |
| 2 | 12 Engine guards | 3 | Rust |
| 2 | 13 Console chaos and limits editor | 1, 8 | console |
| 3 | 14 Admission limits and guard wiring | 2, 10, 12 | Rust |
| 3 | 15 HA change propagation | 10, 11 | Rust |
| 3 | 16 Deployment: Helm chart, Compose profiles, Compose e2e flow | 10 | chart + Rust test |
| 4 | 17 P4 carry-overs: GraphQL/WS caps, stream metering, rejection logging, `?wsdl` | 14 | Rust |
| 4 | 18 Roles, `server.rs`, leader-gated work | 15 | Rust |
| 5 | 19 Chaos | 17 | Rust |
| 5 | 20 TLS termination and outbound CA/proxy | 18 | Rust |
| 5 | 21 `mb-ext` example binary and semver gate | 2, 18 | Rust |
| 6 | 22 Per-stage trace spans and state-size metric | 19 | Rust |
| 7 | 23 CI consolidation and cross-node HA test | 1–22 | workflow + Rust |
| 7 | 24 Operator docs, configuration reference, README | 1–22 | docs + Rust test |

The two critical paths are 4 → 11 → 15 → 18 → 20 → 23 and 1 → 10 → 14 → 17 → 19 → 22 → 23.

---

## File Structure

```
Cargo.toml                                       workspace deps; members += "examples/custom-ext"
crates/mb-store/migrations/{sqlite,postgres}/0300_hardening.sql
crates/mb-store/src/{lib.rs,model.rs,sqlite.rs,postgres.rs}, tests/common/conformance.rs
crates/mb-admin/src/{mocks.rs,scenarios.rs,live.rs,openapi.rs}
crates/mb-ext/src/{lib.rs,hooks.rs,policy.rs,quota.rs,secrets.rs,tenancy.rs,usage.rs,vault.rs}, Cargo.toml
crates/mb-engine/src/{pii.rs,guards.rs,lib.rs,stream.rs,message.rs}
crates/mb-state/src/{redis.rs,bus.rs,leader.rs,lib.rs,memory.rs,sql.rs}
crates/mb-protocols/src/{limits.rs,chaos.rs,call.rs,lib.rs}, graphql/*, grpc/*, soap/mod.rs, http_rest/*, ws.rs, events/{runtime.rs,triggers.rs,mqtt.rs}
crates/mockingbird/src/{server.rs,ha.rs,budget.rs,tls.rs,main.rs,config.rs,file_config.rs,reconcile.rs,runtime.rs,registry.rs,recorder.rs,ops.rs,client.rs,maintenance.rs}
crates/mockingbird/tests/{roles.rs,ha.rs,limits.rs,chaos.rs,tls.rs,repo_community.rs,release_workflow.rs,deploy_artifacts.rs,docs_reference.rs}
crates/mb-evals/{Cargo.toml,src/{main.rs,lib.rs,corpus.rs,report.rs,bench.rs},tests/evals.rs}
crates/mb-console/web/src/pages/mocks/{Channels.tsx,ChannelTriggersEditor.tsx,PublishDialog.tsx,EventFeed.tsx,ChaosLimits.tsx}, api/channels.ts, test/{channels.test.tsx,chaos.test.tsx}
examples/custom-ext/{Cargo.toml,src/main.rs,tests/ext.rs}, examples/gallery/*
deploy/helm/mockingbird/**, deploy/compose/{docker-compose.yml,README.md,mockingbird-ha.yaml}
.github/workflows/{ci.yml,release.yml,evals.yml}, .github/{ISSUE_TEMPLATE/*,pull_request_template.md,dependabot.yml}
scripts/{test-redis.sh,check-helm.sh,check-version.sh,changelog-section.sh}
SECURITY.md CONTRIBUTING.md CODE_OF_CONDUCT.md CHANGELOG.md README.md
docs/{architecture.md,configuration.md,upgrading.md,performance.md,deploy/{local,compose,kubernetes}.md,operations/runbook.md}
docs/examples/mockingbird.full.yaml
```

---

### Task 1: Store: chaos, limits, token usage and request-log lookup, with the Admin API

**Wave 1. Independent.**

**Files:**
- Create: `crates/mb-store/migrations/{sqlite,postgres}/0300_hardening.sql`
- Modify:
  - `crates/mb-store/src/{lib.rs,model.rs,sqlite.rs,postgres.rs}`, `tests/common/conformance.rs`;
  - every `Mock`/`NewMock`/`ScenarioRules` literal the new fields break (`grep -rn "NewMock {\|ScenarioRules {" crates`);
  - `crates/mb-admin/src/{mocks.rs,scenarios.rs}`, `crates/mb-admin/tests/{mocks.rs,scenarios.rs}`;
  - `crates/mb-console/web/src/api/schema.d.ts` (generated).

**Interfaces:**
- Produces (`mb-store` `model.rs`; derives `Serialize, Deserialize, Clone, Debug, PartialEq, Default`). `mb-store` has no utoipa, so `mb-admin` adds the OpenAPI mirrors `ChaosSchema` and `MockLimitsSchema`, as it does with `DataPlaneAuthSchema`:

```rust
pub struct LatencyJitter { pub min_ms: u64, pub max_ms: u64 }
pub struct ChaosSettings {
    #[serde(default)] pub error_rate: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub latency_jitter: Option<LatencyJitter>,
    #[serde(default)] pub drop_stream_rate: f64,
}
impl ChaosSettings { pub fn validate(&self) -> Result<(), String>; pub fn is_noop(&self) -> bool; }
pub struct RateLimit { pub requests_per_second: f64, pub burst: u32 }
pub struct MockLimits {
    #[serde(default, skip_serializing_if = "Option::is_none")] pub rate_limit: Option<RateLimit>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub daily_token_budget: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub max_request_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub max_response_bytes: Option<u64>,
}
impl MockLimits { pub fn validate(&self) -> Result<(), String>; }
// Mock and NewMock gain:
#[serde(default, skip_serializing_if = "Option::is_none")] pub chaos: Option<ChaosSettings>,
#[serde(default)] pub limits: MockLimits,
// ScenarioRules gains:
#[serde(default, skip_serializing_if = "Option::is_none")] pub chaos: Option<ChaosSettings>,
// ControlStore gains:
async fn get_request_log(&self, id: i64) -> Result<Option<RequestLogEntry>, StoreError>;
/// Adds `tokens` to the mock's usage for `day` (UTC) and returns the new total.
async fn add_token_usage(&self, mock_id: Uuid, day: chrono::NaiveDate, tokens: u64) -> Result<u64, StoreError>;
async fn token_usage(&self, mock_id: Uuid, day: chrono::NaiveDate) -> Result<u64, StoreError>;
```

- Validation rules:
  - The ranges are those in the Global Constraints table; `max_request_bytes` and `max_response_bytes` are given in bytes.
  - Messages name the field, e.g. `chaos.error_rate must be between 0 and 1`.
  - `create_mock`/`update_mock` return `StoreError::Invalid` on a failed validation.
- Admin API:
  - `MockView`, `CreateMock` and `PatchMock` gain `chaos` and `limits`. In `PatchMock`, `chaos: null` clears it; `limits` replaces the whole object.
  - The scenario view and `PUT` body gain `rules.chaos`.
  - Invalid values return 422 with the validation message. File-managed mocks keep the existing 409 `managed_by_file`.
  - Updates are audited as the existing `mock.update` / `scenario.put`.
- Migration:
  - `ALTER TABLE mocks ADD COLUMN chaos` (TEXT on SQLite, JSONB on Postgres; nullable).
  - `ALTER TABLE mocks ADD COLUMN limits` (`NOT NULL DEFAULT '{}'`).
  - `CREATE TABLE token_usage (mock_id … REFERENCES mocks(id) ON DELETE CASCADE, day TEXT|DATE, tokens BIGINT NOT NULL, PRIMARY KEY (mock_id, day))`.
  - `add_token_usage` is a single `INSERT … ON CONFLICT DO UPDATE SET tokens = token_usage.tokens + excluded.tokens RETURNING tokens`.

- [ ] **Step 1: Write failing tests**
  - `conformance.rs`:
    - `mock_chaos_and_limits_round_trip`: create with both set, get, update to cleared chaos and changed limits, and both read back equal.
    - `invalid_chaos_rejected`: `error_rate: 1.5` → `Invalid`.
    - `scenario_chaos_round_trip`.
    - `token_usage_accumulates_per_day`: add 100 then 50 on day D → 150; day D+1 → 0.
    - `token_usage_cascades_with_mock`.
    - `get_request_log_by_id`: append → returned id → equal entry; unknown id → `None`.
  - `crates/mb-admin/tests/mocks.rs`:
    - `patch_chaos_and_limits`: 200, and the view shows the values.
    - `patch_invalid_rate_limit_422`: `burst: 0`.
    - `patch_null_chaos_clears`.
  - `crates/mb-admin/tests/scenarios.rs`: `put_scenario_with_chaos`.
- [ ] **Step 2:** Run `cargo test -p mb-store -p mb-admin`. Expected: FAIL (missing fields and methods).
- [ ] **Step 3:** Implement the migration, model, both backends and the Admin API changes.
- [ ] **Step 4:** Run `cargo test -p mb-store -p mb-admin && scripts/test-postgres.sh -p mb-store` (Postgres only where available; report if skipped). Then run `npm --prefix crates/mb-console/web run gen:api && npm --prefix crates/mb-console/web run typecheck`. Expected: PASS.
- [ ] **Step 5:** Commit: `feat(store): chaos, limits, token usage and request-log lookup`.

---

### Task 2: `mb-ext` 1.0 API freeze and `vault:` secret references

**Wave 1. Independent.**

**Files:**
- Create: `crates/mb-ext/src/vault.rs`, `crates/mb-ext/tests/vault.rs`
- Modify:
  - `crates/mb-ext/{Cargo.toml,src/*.rs,tests/defaults.rs}`; `Cargo.toml` (workspace dep `mb-ext = { path = "crates/mb-ext", version = "1.0.0" }`);
  - the call sites that construct `UsageEvent`, `CallInfo` or `Extensions` (`crates/mb-protocols/src/{call.rs,events/produce.rs}`, `crates/mockingbird/src/runtime.rs`, `crates/mb-protocols/tests/{runtime.rs,data_plane_auth_protocols.rs}`, `crates/mockingbird/tests/auth_serve.rs`, `crates/mb-import/src/asyncapi/mod.rs` if it matches).

**Interfaces:**
- Produces:

```rust
// crates/mb-ext/Cargo.toml: version = "1.0.0" (not workspace-inherited); deps += reqwest (workspace), serde_json; publish allowed
#![deny(missing_docs)]                       // lib.rs; every pub item documented, crate docs list the stability policy
#[non_exhaustive] pub enum ExtError { SecretNotFound(String), BadSecretRef(String), Other(String) }
#[non_exhaustive] pub struct CallInfo<'a> { pub mock: &'a str, pub operation_id: &'a str, pub headers: &'a http::HeaderMap }
impl<'a> CallInfo<'a> { pub fn new(mock: &'a str, operation_id: &'a str, headers: &'a http::HeaderMap) -> Self; }
#[non_exhaustive] pub struct UsageEvent { /* fields unchanged */ }
impl UsageEvent { pub fn new(tenancy: Tenancy, mock: impl Into<String>, operation_id: impl Into<String>) -> Self; } // status 0, source "", zero latency/tokens; fields stay pub for assignment
impl Rejection { pub fn new(status: u16, title: impl Into<String>, detail: impl Into<String>) -> Self; pub fn with_retry_after(self, d: Duration) -> Self; }
impl Tenancy { pub fn new(tenant: impl Into<String>, workspace: impl Into<String>) -> Self; }
#[non_exhaustive] pub struct Extensions { /* fields unchanged */ }
impl Extensions {
    pub fn with_tenants(self, t: Arc<dyn TenantResolver>) -> Self;  pub fn with_quota(self, q: Arc<dyn QuotaGuard>) -> Self;
    pub fn with_usage(self, u: Arc<dyn UsageSink>) -> Self;        pub fn with_secrets(self, s: Arc<dyn SecretStore>) -> Self;
    pub fn with_providers(self, p: Arc<dyn ProviderPolicy>) -> Self; pub fn with_admin_hooks(self, h: Arc<dyn AdminHooks>) -> Self;
}
// vault.rs
pub struct VaultSecrets { /* addr, token source, namespace, client, 5-min cache */ }
impl VaultSecrets { pub fn from_env() -> Result<Option<Self>, ExtError>; }   // None when VAULT_ADDR unset
pub struct DefaultSecrets { /* EnvFileSecrets + Option<VaultSecrets> */ }
impl DefaultSecrets { pub fn from_env() -> Self; }   // a bad Vault env is logged and Vault disabled
// Extensions::default().secrets = DefaultSecrets::from_env()
```

- **`vault:` reference format:** `vault:<mount>/<path>#<field>`. KV v2: `GET {VAULT_ADDR}/v1/<mount>/data/<path>` with `X-Vault-Token`, plus `X-Vault-Namespace` when `VAULT_NAMESPACE` is set. The value is `.data.data.<field>`, which must be a string.
  - The token comes from `VAULT_TOKEN`, else from the file named by `VAULT_TOKEN_FILE`, re-read on every cache miss.
  - `VAULT_CACERT` adds a PEM root. The timeout is 5 s.
  - 404 or a missing field → `SecretNotFound`. Any other failure → `Other` naming the reference, **never the value**.
  - Values are cached for 5 minutes.
- **AWS/GCP/Azure secret managers are not implemented.** The crate docs and Task 24 explain the Secrets Store CSI driver route (secrets mounted as files, read with `file:`). See the spec gaps.
- `Rejection` and `Tenancy` stay exhaustive: implementations build them, and adding a field is a 2.0 change. The crate docs say so, and they also say that `axum` 0.8 and `http` 1 types are part of the public API.

- [ ] **Step 1: Write failing tests**
  - `tests/defaults.rs`: `builders_replace_one_extension`, `rejection_new_and_retry_after`, `usage_event_new_defaults`.
  - `tests/vault.rs` (wiremock):
    - `vault_reads_kv2_field` (asserts the request path `/v1/secret/data/mockingbird`, the token header and the namespace header);
    - `vault_missing_field_is_not_found`;
    - `vault_403_is_other_without_value`;
    - `vault_token_file_reread_after_cache_expiry` (paused tokio time);
    - `default_secrets_without_vault_addr_rejects_vault_refs` (`BadSecretRef`).
- [ ] **Step 2:** Run `cargo test -p mb-ext`. Expected: FAIL.
- [ ] **Step 3:** Implement. Document every item. Migrate the call sites to the constructors and builders (`cargo build --workspace --all-features --all-targets` finds them).
- [ ] **Step 4:** Run `RUSTDOCFLAGS="-D warnings" cargo doc -p mb-ext --no-deps && cargo test --workspace --all-features`. Expected: PASS.
- [ ] **Step 5:** Commit: `feat(ext): mb-ext 1.0 API, documented and non-exhaustive, with vault: secrets`.

---

### Task 3: PII scrubber module

**Wave 1. Independent.**

**Files:**
- Create: `crates/mb-engine/src/pii.rs`, `crates/mb-engine/tests/pii.rs`
- Modify: `crates/mb-engine/{Cargo.toml (regex, serde),src/lib.rs (pub mod pii)}`, `Cargo.toml` (`regex`)

**Interfaces:**
- Produces:

```rust
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)] #[serde(rename_all = "snake_case")]
pub enum BuiltinPattern { Email, Phone, CreditCard, UsSsn, Iban, Ipv4 }
impl BuiltinPattern { pub const ALL: [BuiltinPattern; 6]; pub fn as_str(self) -> &'static str; }
#[derive(Deserialize, Clone, Debug, PartialEq)] pub struct CustomPattern { pub name: String, pub regex: String, pub replacement: String }
#[derive(Clone, Debug, PartialEq)] pub struct ScrubberConfig { pub builtins: Vec<BuiltinPattern>, pub custom: Vec<CustomPattern> }
#[derive(thiserror::Error, Debug)] pub enum PiiError { #[error("pii.custom[{index}]: {reason}")] Custom { index: usize, reason: String } }
pub struct Scrubber { /* compiled */ }
impl Scrubber {
    pub fn new(cfg: &ScrubberConfig) -> Result<Self, PiiError>;
    /// Replaces matches inside every JSON string (object keys untouched); returns replacements per pattern name.
    pub fn scrub_value(&self, v: &mut serde_json::Value) -> BTreeMap<String, usize>;
    pub fn scrub_str(&self, s: &str) -> (String, BTreeMap<String, usize>);
}
```

**Detection and replacement.** Each builtin's replacement falls in its excluded range, so scrubbing is idempotent.

| Pattern | Detects | Excluded (never replaced) | Replacement |
|---|---|---|---|
| `email` | RFC-5322-ish `local@domain.tld` | domains `example.com/.net/.org`, `*.example`, `*.test`, `*.invalid` | `user` + 6 random lowercase letters + `@example.com` |
| `phone` | NANP with a separator or parentheses (`(415) 555-2671`, `415-555-2671`, `415.555.2671`, optional `+1 `), or E.164 `+[1-9]\d{7,14}` | exchange `555` with line `0100`–`0199` | same punctuation and length, with exchange `555` and line `01DD`; E.164 → `+1555010` plus random digits to the same length |
| `credit_card` | 13–19 digits, optionally in groups split by single spaces or hyphens, Luhn-valid, not all one digit | BIN `400000` | `400000` + random digits + Luhn check digit, keeping the separator positions |
| `us_ssn` | `AAA-GG-SSSS`, area not `000`/`666`/`9xx`, group ≠ `00`, serial ≠ `0000` | area `9xx` | `9DD-DD-DDDD` |
| `iban` | 2 letters + 2 digits + 11–30 alphanumerics (spaces in groups of 4 allowed), mod-97 valid | BBAN starting with `0000` | same country and length: BBAN `0000` + random digits, check digits recomputed |
| `ipv4` | dotted quad, each octet 0–255, word-bounded | `192.0.2.0/24`, `198.51.100.0/24`, `203.0.113.0/24`, `0.0.0.0/8`, `10/8`, `127/8`, `172.16/12`, `192.168/16` | random host in one of the three documentation ranges |

- Custom patterns run after the builtins. Their replacement template is expanded per match.
- Strings longer than 64 KiB are left as they are.

- [ ] **Step 1: Write failing tests** (`tests/pii.rs`):
  - one per builtin: `replaces_email`, `replaces_nanp_and_e164_phones`, `replaces_luhn_valid_cards_keeping_separators`, `replaces_ssn`, `replaces_valid_iban`, `replaces_public_ipv4`. Each asserts that the output still matches the same pattern's shape regex and lies in the excluded range.
  - `leaves_uuids_dates_and_order_numbers`: `3f2b…` UUID, `2026-10-14`, `ORD-1234567890`, `v1.2.3.4` and 10 bare digits stay unchanged.
  - `luhn_invalid_digits_untouched`.
  - `scrub_is_idempotent`: scrubbing twice equals scrubbing once, and the second pass counts zero.
  - `keys_are_not_scrubbed`.
  - `custom_pattern_template`: `EMP-\d{6}` with `EMP-######` → `EMP-` plus 6 digits.
  - `invalid_custom_regex_names_index`.
  - `disabled_builtins_are_skipped`.
  - proptest `scrub_never_panics_and_is_idempotent` over arbitrary strings.
- [ ] **Step 2:** Run `cargo test -p mb-engine --test pii`. Expected: FAIL.
- [ ] **Step 3:** Implement with `regex::RegexBuilder` (size limit 1 MiB) and `rand`.
- [ ] **Step 4:** Run `cargo test -p mb-engine`. Expected: PASS.
- [ ] **Step 5:** Commit: `feat(engine): PII scrubber with synthetic same-shape replacements`.

---

### Task 4: Redis state store

**Wave 1. Independent.**

**Files:**
- Create: `crates/mb-state/src/redis.rs`, `crates/mb-state/tests/redis.rs`, `scripts/test-redis.sh`
- Modify: `crates/mb-state/{Cargo.toml (redis, uuid),src/lib.rs}`, `Cargo.toml` (`redis`)

**Interfaces:**
- Produces:

```rust
pub struct RedisStateStore { /* ConnectionManager, ttl, key prefix "mb:", broadcast::Sender<StateChanged> */ }
impl RedisStateStore { pub async fn connect(url: &str, ttl: Duration) -> Result<Self, StateError>; }
impl StateStore for RedisStateStore { /* full trait */ }
// lib.rs connect(): "redis://" | "rediss://" → RedisStateStore
```

**Key layout.** `{s}` is `mb:{workspace}:{mock}:{session}`; names are restricted to `[A-Za-z0-9._-]`, so `:` is a safe separator.
- `{s}:res`: hash with field `{resource_type}\x1f{id}` → JSON `StoredResource`.
- `{s}:order`: sorted set of the same fields, scored by a global `INCR mb:seq`. It gives recency for `list` (filtered by type prefix, newest first) and `export` (oldest first).
- `{s}:meta`: hash with `last_touched` (RFC 3339).
- `mb:{workspace}:{mock}:sessions`: sorted set of session ids, scored by `last_touched` in ms.

**Write and expiry rules.**
- Every write and every `touch` runs in one `MULTI` pipeline. It also `PEXPIRE`s the three session keys to the TTL and updates the sessions zset.
- `sessions()` drops ids whose score is older than the TTL (`ZREMRANGEBYSCORE`).
- `reset_mock` deletes every listed session's keys, then the zset.
- `touch` on a missing `{s}:meta` is a no-op.
- `seed` replaces the session atomically.
- Change events go to the local broadcast only; Task 15 adds the cross-node path.
- `ping` is `PING` with a 2 s timeout.

- [ ] **Step 1: Write failing tests.** `tests/redis.rs` runs `common::conformance::run_all` and `ttl_expiry` (the existing suite) against `MB_TEST_REDIS_URL`, each test under its own random workspace name. It adds `redis_reset_mock_removes_all_sessions` and `redis_survives_reconnect` (drop the store, connect again, data still there). `scripts/test-redis.sh` picks a free port, starts `redis-server --port <p> --save "" --appendonly no` in a temp dir, and stops it on exit.
- [ ] **Step 2:** Run `scripts/test-redis.sh -p mb-state --test redis`. Expected: FAIL to compile.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Run `scripts/test-redis.sh -p mb-state`. Expected: PASS. Without the variable, `cargo test -p mb-state` prints `skipped:` and passes.
- [ ] **Step 5:** Commit: `feat(state): Redis state backend`.

---

### Task 5: Community and repository files, and the example gallery

**Wave 1. Independent.**

**Files:**
- Create:
  - `SECURITY.md`, `CONTRIBUTING.md`, `CODE_OF_CONDUCT.md`, `CHANGELOG.md`;
  - `.github/ISSUE_TEMPLATE/{bug_report.yml,feature_request.yml,config.yml}`, `.github/pull_request_template.md`, `.github/dependabot.yml`;
  - `examples/gallery/{README.md,rest/mockingbird.yaml,graphql/mockingbird.yaml,grpc/mockingbird.yaml,soap/mockingbird.yaml,events/mockingbird.yaml,chaos/mockingbird.yaml}`;
  - `scripts/changelog-section.sh`, `crates/mockingbird/tests/repo_community.rs`.

**Content rules:**
- **SECURITY.md:**
  - supported versions (latest minor);
  - private reporting through GitHub Security Advisories ("Report a vulnerability");
  - a 3-business-day acknowledgement and a 90-day disclosure window;
  - scope (core repository only);
  - how releases are signed and verified: `cosign verify ghcr.io/<owner>/mockingbird:<v> --certificate-identity-regexp 'https://github.com/<owner>/mockingbird/.github/workflows/release.yml@refs/tags/v.*' --certificate-oidc-issuer https://token.actions.githubusercontent.com`, and `gh attestation verify`.
- **CONTRIBUTING.md:**
  - toolchain 1.97, Node 22;
  - the local gates, which are the exact CI commands (fmt, clippy, tests, `scripts/test-postgres.sh`, `scripts/test-redis.sh`, the console scripts);
  - the commit style (`type(scope): summary`);
  - the DCO-free inbound=outbound license statement (`MIT OR Apache-2.0`);
  - how to add a protocol adapter or importer, and the rule that `mb-ext` changes need a semver review.
- **CODE_OF_CONDUCT.md:** Contributor Covenant 2.1, with the enforcement contact given as the GitHub Security Advisories / maintainers channel. Do not invent an email address.
- **CHANGELOG.md:** keep-a-changelog 1.1.0. It has `## [Unreleased]` and `## [0.1.0] - Unreleased`, with one `### Added` bullet per phase P1–P7 summarising §16.
- **`scripts/changelog-section.sh <version>`** prints that version's section body. Exit 1 if it is missing.
- **dependabot:** `cargo` (weekly, grouped minor/patch), `npm` at `/crates/mb-console/web` (weekly, grouped), `github-actions` (weekly).
- **Gallery:** each `mockingbird.yaml` uses only existing fixtures (`../../../fixtures/...`) and existing keys, plus `chaos` in `chaos/`. The chaos example is added in Task 10 because its keys do not exist yet; in this task `chaos/` holds only a README stub. `examples/gallery/README.md` has a table of the examples with a `mockingbird serve -c …` line for each.

- [ ] **Step 1: Write the failing test** `repo_community.rs`:
  - `community_files_exist`: each file is non-empty and SECURITY.md contains `Report a vulnerability`.
  - `changelog_has_0_1_0_section`: runs `scripts/changelog-section.sh 0.1.0` and asserts that P1 through P7 are mentioned.
  - `dependabot_covers_three_ecosystems`: parsed with `serde_saphyr` into `serde_json::Value`.
  - `gallery_configs_load`: every `examples/gallery/*/mockingbird.yaml` except `chaos` loads through `mockingbird::file_config::load`.
- [ ] **Step 2:** Run `cargo test -p mockingbird --test repo_community`. Expected: FAIL.
- [ ] **Step 3:** Write the files.
- [ ] **Step 4:** Run the test again. Expected: PASS.
- [ ] **Step 5:** Commit: `docs: security policy, contributing guide, code of conduct, changelog, templates, example gallery`.

---

### Task 6: Release pipeline

**Wave 1. Independent.** CI-verified, with a local YAML gate.

**Files:**
- Create: `.github/workflows/release.yml`, `scripts/check-version.sh`, `crates/mockingbird/tests/release_workflow.rs`
- Modify: `Dockerfile` (OCI labels via `ARG VERSION`, `ARG REVISION`; `LABEL org.opencontainers.image.{source,version,revision,licenses="MIT OR Apache-2.0"}`)

**Workflow `release.yml`** (`on: push: tags: ['v*']` and `workflow_dispatch` with input `dry_run` (default true; when it is true nothing is pushed or published)):
- Top-level `permissions: contents: read`. Each job raises only what it needs: `contents: write` (release), `packages: write`, `id-token: write`, `attestations: write`.
- **`verify`:** `scripts/check-version.sh "$GITHUB_REF_NAME"`. It fails unless:
  - the tag `vX.Y.Z` equals the workspace `version`;
  - `deploy/helm/mockingbird/Chart.yaml` has `appVersion: "X.Y.Z"` (skipped with a notice while the chart does not exist);
  - `scripts/changelog-section.sh X.Y.Z` succeeds.
- **`console`:** builds `crates/mb-console/dist` and uploads it as an artifact; the later jobs download it before `cargo build`.
- **`binaries`** (matrix, `fail-fast: false`, default features `mqtt amqp`; Kafka ships only in the full image):

| Target | Runner | Build |
|---|---|---|
| `x86_64-unknown-linux-musl` | ubuntu-24.04 | `cargo zigbuild --release --locked` |
| `aarch64-unknown-linux-musl` | ubuntu-24.04 | `cargo zigbuild --release --locked` |
| `aarch64-apple-darwin` | macos-14 | `cargo build --release --locked` |
| `x86_64-apple-darwin` | macos-13 | `cargo build --release --locked` |
| `x86_64-pc-windows-msvc` | windows-latest | `cargo build --release --locked` |

  Archives are `mockingbird-<version>-<target>.tar.gz` (`.zip` on Windows), with the binary, both licenses, README.md and CHANGELOG.md. Each archive gets a `.sha256`. `actions/attest-build-provenance@v3` attests them with `subject-path`.
- **`sbom`:** `cargo cyclonedx --format json --spec-version 1.5 --all-features --target all` writes `mockingbird-<version>.cdx.json`.
- **`images`:**
  - Matrix `{arch: amd64, runner: ubuntu-24.04}`, `{arch: arm64, runner: ubuntu-24.04-arm}` × `{flavor: full, slim}`; slim uses `FEATURES=--no-default-features`.
  - Each `docker/build-push-action@v6` builds and pushes by digest to `ghcr.io/${{ github.repository_owner }}/mockingbird` (`outputs: type=image,push-by-digest=true,name-canonical=true,push=true`).
  - `aquasecurity/trivy-action` scans each digest with `severity: CRITICAL,HIGH`, `exit-code: 1`, `ignore-unfixed: true`.
  - The digests are uploaded as artifacts.
- **`manifest`** (needs `images`):
  - `docker buildx imagetools create` builds the tags `<v>` and `<v>-slim`, plus `latest` and `latest-slim` when the tag has no `-` (not a prerelease).
  - `cosign sign --yes <image>@<digest>` (keyless) for both manifests.
  - `actions/attest-build-provenance@v3` with `subject-name`, `subject-digest` and `push-to-registry: true`.
  - `actions/attest-sbom@v3` with the CycloneDX file.
- **`chart`:** if `deploy/helm/mockingbird` exists, `helm package` it, `helm push` to `oci://ghcr.io/<owner>/charts` and `cosign sign` the chart digest.
- **`publish-crate`:** `cargo publish -p mb-ext --locked` with `CARGO_REGISTRY_TOKEN`. It is skipped when `cargo info mb-ext@<mb-ext version>` already resolves on crates.io.
- **`release`:** `gh release create "$TAG" --verify-tag --notes-file <(scripts/changelog-section.sh X.Y.Z)` with the archives, `SHA256SUMS` and the SBOM. `--prerelease` when the tag contains `-`.
- Every `uses:` is pinned to at least a major tag.

- [ ] **Step 1: Write the failing test** `release_workflow.rs`:
  - `release_jobs_present`: the job ids `verify console binaries sbom images manifest chart publish-crate release`.
  - `binary_targets_exact`: the set of 5 targets above.
  - `images_scan_and_sign`: a step uses `aquasecurity/trivy-action` with `CRITICAL,HIGH` and `exit-code: 1`; the manifest job runs `cosign sign`; `attest-build-provenance` and `attest-sbom` are used.
  - `oidc_permission_scoped`: `id-token: write` appears only at job level.
  - `every_action_pinned`.
  - `check_version_script`: `bash scripts/check-version.sh v0.1.0` → 0; `v9.9.9` → 1 (`#[cfg(unix)]`).
- [ ] **Step 2:** Run `cargo test -p mockingbird --test release_workflow`. Expected: FAIL.
- [ ] **Step 3:** Write the workflow, the script and the Dockerfile labels.
- [ ] **Step 4:** Run the test again. Expected: PASS. Also run `actionlint` if it is installed (optional).
- [ ] **Step 5:** Commit: `ci: signed multi-arch release with SBOM, provenance, binaries and mb-ext publish`.

---

### Task 7: `mb-evals`: nightly LLM evals and the offline throughput bench

**Wave 1. Independent.**

**Files:**
- Create: `crates/mb-evals/{Cargo.toml,src/{main.rs,lib.rs,corpus.rs,report.rs,bench.rs},tests/evals.rs}`, `.github/workflows/evals.yml`

**Interfaces:**
- Produces (`publish = false`; deps: `mb-import`, `mb-engine`, `mb-llm`, `mb-mcm`, `mockingbird` (lib), `clap`, `tokio`, `reqwest`, `serde`, `serde_json`, `humantime`, `anyhow`, `chrono`):

```rust
// corpus.rs
pub struct CorpusEntry { pub name: String /* path relative to corpus root */, pub bundle: mb_mcm::Bundle }
pub fn load(root: &Path) -> anyhow::Result<Vec<CorpusEntry>>;   // every file `mb_import::compile` accepts, sorted; unreadable/failed → skipped with a warning; dirs compiled via archive path when they hold several protos
// report.rs
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct EvalReport { pub model: String, pub base_url: String, pub started_at: chrono::DateTime<chrono::Utc>,
    pub calls: u64, pub schema_valid_rate: f64 /* (llm+repaired)/calls */, pub first_pass_rate: f64 /* llm/calls */,
    pub repair_rate: f64, pub fallback_rate: f64, pub p50_ms: u64, pub p95_ms: u64,
    pub prompt_tokens_per_call: f64, pub completion_tokens_per_call: f64,
    pub per_contract: BTreeMap<String, ContractStats> }
pub struct ContractStats { pub calls: u64, pub llm: u64, pub repaired: u64, pub fallback: u64, pub p95_ms: u64 }
pub struct Thresholds { pub min_schema_valid_rate: f64 /* 0.95 */, pub max_repair_rate: f64 /* 0.25 */, pub max_fallback_rate: f64 /* 0.05 */, pub max_p95_ms: u64 /* 3000 */ }
impl EvalReport { pub fn violations(&self, t: &Thresholds) -> Vec<String>; pub fn to_markdown(&self) -> String; }
pub async fn run_evals(entries: &[CorpusEntry], provider: Arc<dyn mb_llm::Provider>, model: &str, max_ops: usize) -> EvalReport;
// bench.rs
pub struct BenchReport { pub requests: u64, pub errors: u64, pub rps: f64, pub p50_ms: f64, pub p95_ms: f64, pub p99_ms: f64, pub concurrency: usize, pub duration_s: f64 }
pub async fn bench(contract: &Path, duration: Duration, concurrency: usize) -> anyhow::Result<BenchReport>;
```

**CLI and workflow:**
- `mb-evals run [--corpus fixtures] [--out eval-report] [--max-ops-per-contract 10] [--min-schema-valid-rate …] [--max-repair-rate …] [--max-fallback-rate …] [--max-p95-ms …]`
  - It reads `MB_EVAL_LLM_BASE_URL`, `MB_EVAL_LLM_MODEL` and `MB_EVAL_LLM_API_KEY`; a missing one is a usage error (exit 2).
  - It evaluates unary operations with a success schema and publish channels (`Engine::generate_message`). Each call gets a faker-built input.
  - It writes `report.json` and `report.md`. Exit code 1 when any threshold is violated, after writing the reports.
- `mb-evals bench [--contract fixtures/openapi/orders-3.1.yaml] [--duration 10s] [--concurrency 32] [--out bench.json]`
  - It starts `mockingbird::config::build_app` offline (`--offline`, SQLite in memory) on `127.0.0.1:0`.
  - It drives a mix of the contract's GET and POST operations with `reqwest`, then prints and writes the `BenchReport`.
- **`evals.yml`:**
  - triggers: `schedule: cron "17 3 * * *"` and `workflow_dispatch`;
  - runs `cargo run --release -p mb-evals -- run --out eval-report`, with secrets passed as env;
  - uploads `eval-report/` with `if: always()` and appends `report.md` to `$GITHUB_STEP_SUMMARY`;
  - `permissions: contents: read`. It never runs on `pull_request`.

- [ ] **Step 1: Write failing tests** (`tests/evals.rs`):
  - `corpus_loads_fixture_contracts` (≥ 10 entries, including `openapi/orders-3.1.yaml` and `asyncapi/streetlights-kafka-3.0.yaml`).
  - `run_evals_with_scripted_provider_counts_sources`: `mb_llm::scripted` returns a valid body first, then an invalid one, then a valid one, so llm, repaired and fallback are all non-zero and the rates sum to 1.
  - `thresholds_report_violations`.
  - `markdown_has_summary_table`.
  - `run_without_env_is_usage_error`: `assert_cmd`, exit 2.
  - `bench_offline_smoke`: `bench(…, 1s, 4)` → `requests > 0` and `errors == 0`.
- [ ] **Step 2:** Run `cargo test -p mb-evals`. Expected: FAIL.
- [ ] **Step 3:** Implement the crate and the workflow.
- [ ] **Step 4:** Run `cargo test -p mb-evals && cargo run --release -p mb-evals -- bench --duration 10s --out target/bench.json`. Expected: PASS. Note the printed numbers in the commit body; Task 24 copies them into `docs/performance.md`.
- [ ] **Step 5:** Commit: `feat(evals): nightly LLM evals and offline throughput bench`.

---

### Task 8: Console Channels tab

**Wave 1. Independent.** (`web/` = `crates/mb-console/web/`)

**Files:**
- Create: `web/src/api/channels.ts`, `web/src/pages/mocks/{Channels.tsx,ChannelTriggersEditor.tsx,PublishDialog.tsx,EventFeed.tsx}`, `web/src/test/channels.test.tsx`
- Modify:
  - `crates/mb-admin/src/live.rs`: `StreamQuery` gains `kind: Option<String>`, which filters on `RequestRecord.kind`. An unknown kind is 400, matching `requests::list`.
  - `crates/mb-admin/tests/live.rs` (or the existing live test file);
  - `web/src/api/{schema.d.ts,sse.ts}`, `web/src/router.tsx`, `web/src/pages/mocks/MockLayout.tsx`, `web/e2e/runtime.spec.ts`.

**Interfaces:**
- Consumes the P5 endpoints under `/api/v1/workspaces/{ws}/mocks/{mock}/`:
  - `channels`, `channels/{channel}` (GET/PUT), `channels/{channel}/publish`, `event-settings` (GET/PUT);
  - `/api/v1/brokers`;
  - `requests/stream?kind=event`.
- Produces:

```ts
export type ChannelView = components["schemas"]["ChannelView"];
export type ChannelDetail = components["schemas"]["ChannelDetail"];
export type TriggerDraft =
  | { kind: "schedule"; every_ms: number } | { kind: "cron"; expr: string }
  | { kind: "on_state_change"; resource_type: string; change: "upserted" | "deleted" | "any" } | { kind: "manual" };
export function toMcmTrigger(t: TriggerDraft): unknown;      // → {"schedule":{"every_ms":…}} | {"cron":{"expr":…}} | {"on_state_change":{…}} | "manual"
export function fromMcmTrigger(v: unknown): TriggerDraft | null;
export function useChannels(ws: string, mock: string);       // refetchInterval 5000 (activity counters)
export function useChannel(ws: string, mock: string, id: string | undefined);
export function usePutChannel(ws: string, mock: string);     // ({id, enabled, triggers: unknown[] | null})
export function usePublish(ws: string, mock: string);       // ({id, count, context?}) → PublishResult
export function useEventSettings(ws: string, mock: string); export function usePutEventSettings(ws: string, mock: string);
export function useBrokers();
```

**Behaviour:**
- **Tab:** route `/w/$ws/mocks/$mock/channels`, labelled "Channels", placed after "Requests".
  - Empty state: `This mock's active contract declares no channels.`
  - **Channels table** (`aria-label="Channels"`) columns: Channel (id and title), Direction (`publish`/`subscribe`), Protocol, Address, Enabled, Triggers (summarised, e.g. `every 5s`, `cron 0 * * * *`, `on order upserted`, `manual`), Activity (published/received counts and the last error, in an alert colour **plus** a text label).
- **Row actions** (Edit permission; disabled with the read-only reason for file mocks and viewers):
  - **Enable switch.**
  - **"Edit triggers":** a drawer for publish channels only. A list of `TriggerDraft` rows with add/remove. Validation:
    - `every_ms` ≥ 100 (message `at least 100 ms`);
    - cron is non-empty;
    - `resource_type` is required.

    The checkbox "Use the contract's triggers" sends `triggers: null`. A 422 from the server is shown with `ProblemAlert`.
  - **"Publish now":** for publish channels only. A dialog with Count (1–100) and Context (a JSON object textarea; parse errors block Submit). On success it lists every published message (address, key, `source` badge, pretty payload in `<pre aria-label="Published payload">`). A 409 or 502 problem is shown inline.
- **Broker selector:** above the table. "Automatic" plus each broker from `/brokers` with its status, saved through `event-settings`.
- **Event feed** (`EventFeed`, `aria-label="Event feed"`): an SSE subscription to `requests/stream?kind=event`. Newest first, capped at 200 rows: time, channel (`operation_id`), `PUBLISH`/`RECEIVE`, status, source. There is a Pause toggle.

- [ ] **Step 1: Write failing tests:**
  - Rust (`mb-admin`): `live_stream_filters_kind` and `live_stream_unknown_kind_400`.
  - Vitest `channels.test.tsx`:
    - `lists channels with trigger summaries and activity`;
    - `toggle enabled sends put`;
    - `trigger editor round trips mcm triggers`;
    - `trigger editor rejects schedule under 100ms`;
    - `use contract triggers sends null`;
    - `publish dialog shows published payloads`;
    - `publish context must be a json object`;
    - `broker selector saves event settings`;
    - `event feed shows streamed event records`;
    - `file mock is read only`;
    - `empty state without channels`.
  - Playwright `runtime.spec.ts` gains `channels tab publishes on a websocket channel`, using `fixtures/asyncapi/chat-ws-2.6.yaml` with the memory/WS path: publish 1 → the feed shows `PUBLISH`.
- [ ] **Step 2:** Run `cargo test -p mb-admin live && npm --prefix crates/mb-console/web test`. Expected: FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Run `npm --prefix crates/mb-console/web run typecheck && npm --prefix crates/mb-console/web run lint && npm --prefix crates/mb-console/web test && npm --prefix crates/mb-console/web run build && npm --prefix crates/mb-console/web run check:api`, then `MB_E2E_CHROMIUM=/opt/pw-browsers/chromium-1194/chrome-linux/chrome npm --prefix crates/mb-console/web run e2e`. Expected: all pass.
- [ ] **Step 5:** Commit: `feat(console): channels tab with triggers, publish and live event feed`.

---

### Task 9: CLI `--workspace`, `scenarios` and `profiles`

**Wave 1. Independent.**

**Files:**
- Modify: `crates/mockingbird/src/{client.rs,config.rs (Cli)}`, `crates/mockingbird/tests/client.rs`

**Interfaces:**
- Produces:

```rust
// Cli gains:
#[arg(long, global = true, env = "MB_WORKSPACE", default_value = mb_store::DEFAULT_WORKSPACE)] pub workspace: String,
// client.rs
pub async fn run(admin_url: &str, token: Option<&str>, workspace: &str, cmd: ClientCommand) -> Result<Option<Value>, ClientError>;
impl AdminClient { fn ws_url(&self, segments: &[&str]) -> Url; }   // <base>/api/v1/workspaces/<ws>/<segments…>
// ClientCommand gains:
#[command(subcommand)] Scenarios(ScenariosCommand),   // list <mock> | get <mock> <name> | put <mock> <name> --file <json> | delete <mock> <name>
#[command(subcommand)] Profiles(ProfilesCommand),     // list | get <name> | put <name> --file <json> | delete <name>
```

**Rules:**
- Every mock-scoped and workspace-scoped call uses `ws_url`: mocks, import (both the contract upload and P6's smart import and its polling), sessions, export/seed fixtures, channels, event settings, scenarios, profiles.
- `brokers list` keeps `/api/v1/brokers`.
- `scenarios get` filters the list response client-side, because there is no single-scenario GET. An unknown name is a usage error: `scenario <name> not found in mock <mock>`.
- `--file -` reads stdin. Bodies are sent as JSON.

- [ ] **Step 1: Write failing tests** (`tests/client.rs`, against an in-process server like the existing tests):
  - `workspace_flag_targets_workspace`: create workspace `team-a` over HTTP, then `--workspace team-a mocks create m1`; `mocks list` shows it in `team-a` and not in `default`.
  - `mb_workspace_env_is_honoured`.
  - `scenarios_put_list_delete`.
  - `scenarios_get_unknown_is_usage_error` (exit 2).
  - `profiles_put_get_delete`.
  - `brokers_list_stays_global`.
  - The existing client tests still pass on `default`.
- [ ] **Step 2:** Run `cargo test -p mockingbird --test client`. Expected: FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Run `cargo test -p mockingbird --test client`. Expected: PASS.
- [ ] **Step 5:** Commit: `feat(cli): --workspace/MB_WORKSPACE and scenarios/profiles subcommands`.

---

### Task 10: Config surface and validation

**Wave 2. Depends on Task 1.**

**Files:**
- Modify:
  - `crates/mockingbird/src/{file_config.rs,config.rs,reconcile.rs}`, `crates/mb-protocols/src/lib.rs` (`ProtocolSettings` fields only);
  - `crates/mb-admin/src/scenarios.rs` (the 30 s `stream_item_interval_ms` cap → 422);
  - `crates/mockingbird/tests/config.rs`, `examples/gallery/chaos/mockingbird.yaml`, `docs/examples/mockingbird.yaml`.

**Interfaces:**
- Produces (`file_config.rs`; every struct `#[serde(deny_unknown_fields, default)]`):

```rust
#[derive(Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)] #[serde(rename_all = "lowercase")]
pub enum Role { #[default] All, Control, Data }
// ServerConfig gains: pub role: Role, pub trust_forwarded_headers: bool, pub tls: Option<TlsConfig>
pub struct TlsConfig { pub cert_file: PathBuf, pub key_file: PathBuf }
pub struct OutboundConfig { pub ca_file: Option<PathBuf>, pub proxy: Option<String>, pub no_proxy: Vec<String> }
pub struct BusConfig { pub url: Option<String> }
pub struct LimitsConfig { pub max_concurrent_generations: usize, pub max_request_bytes: u64, pub max_response_bytes: u64 }
pub struct PiiConfig { pub enabled: bool, pub patterns: Vec<mb_engine::pii::BuiltinPattern>, pub custom: Vec<mb_engine::pii::CustomPattern> }
// FileConfig gains: pub outbound: OutboundConfig, pub bus: BusConfig, pub limits: LimitsConfig, pub pii: PiiConfig
// GraphqlConfig gains: max_root_fields, max_subscriptions_per_connection, max_connections_per_mock
pub struct FileChaos { pub error_rate: f64, pub latency_jitter: Option<FileJitter>, pub drop_stream_rate: f64 }
pub struct FileJitter { #[serde(with = "humantime_serde")] pub min: Duration, #[serde(with = "humantime_serde")] pub max: Duration }
pub struct FileRateLimit { pub requests_per_second: f64, pub burst: u32 }
pub struct FileMockLimits { pub rate_limit: Option<FileRateLimit>, pub daily_token_budget: Option<u64>, pub max_request_bytes: Option<u64>, pub max_response_bytes: Option<u64> }
// FileMock gains: pub chaos: Option<FileChaos>, pub limits: FileMockLimits; FileScenario gains: pub chaos: Option<FileChaos>
impl FileChaos { pub fn to_store(&self) -> mb_store::ChaosSettings; }
impl FileMockLimits { pub fn to_store(&self) -> mb_store::MockLimits; }
pub enum BusTarget { Local, Postgres(String), Redis(String) }
impl FileConfig { pub fn bus_target(&self) -> BusTarget; pub fn scrubber_config(&self) -> Option<mb_engine::pii::ScrubberConfig>; /* None unless pii.enabled */ }
// ServeArgs gains: #[arg(long, value_enum)] pub role: Option<Role>
// mb-protocols ProtocolSettings gains (Default: 20, 16, 1000, false, false):
pub graphql_max_root_fields: usize, pub graphql_max_subscriptions_per_connection: usize,
pub graphql_max_connections_per_mock: usize, pub trust_forwarded_headers: bool, pub tls_terminated: bool,
```

**Rules:**
- `PiiConfig.patterns` defaults to `BuiltinPattern::ALL`. `BusTarget` resolution:
  - `bus.url == "local"` → `Local`;
  - `redis(s)://` → `Redis`;
  - `postgres(ql)://` → `Postgres`;
  - unset → `Postgres(storage.url)` when `storage.url` is Postgres, else `Local`.
- New env vars, applied in `apply_env_from`:
  - `MB_ROLE` (`all|control|data`);
  - `MB_STATE_URL` → `storage.state_url`;
  - `MB_BUS_URL`.

  `--role` goes in `apply_flags`.
- `validate_ranges()` gains checks for every range in the Global Constraints table, each naming the key. Examples:
  - `limits.max_concurrent_generations 0 must be between 1 and 4096`;
  - `protocols.sse.keepalive must be at least 1s`;
  - `protocols.graphql.path must start with '/'`;
  - `mocks.orders.chaos.error_rate must be between 0 and 1` (Task 1's `validate` with the key prefix added).

  It also gains:
  - the role/storage rule (`server.role data requires a postgres:// storage.url; SQLite deployments are single-node` and `server.role data requires a shared state store (storage.state_url postgres:// or redis://)`);
  - TLS both-or-neither;
  - a bus URL scheme check;
  - `Scrubber::new` (pii errors are passed through);
  - `outbound.proxy` must parse as an http(s) URL.
- `protocols.settings()` fills the new `ProtocolSettings` fields. `tls_terminated` = `server.tls.is_some()`.
- Reconcile writes `chaos`/`limits` to stored mocks and `rules.chaos` to scenarios. File-declared values replace whatever the API set, as for every file-managed field today.

- [ ] **Step 1: Write failing tests** (`tests/config.rs`):
  - `p7_sections_default`;
  - `role_flag_env_file_precedence`: file `data` < `MB_ROLE=control` < `--role all`;
  - `split_roles_require_shared_stores`: SQLite and memory state each give an error naming the key;
  - `bus_target_resolution`: the four cases;
  - `limits_ranges_named`;
  - `protocols_validation`: keepalive `500ms`, path `graphql`, `max_message_bytes: 100`, `item_interval: 31s` each give an error naming its key;
  - `mock_chaos_and_limits_reconciled`: serve the config, then the stored mock shows both values;
  - `scenario_chaos_reconciled`;
  - `pii_custom_invalid_regex_rejected`;
  - `tls_requires_both_files`;
  - `unknown_p7_key_rejected`.
  - `crates/mb-admin/tests/scenarios.rs`: `put_scenario_interval_over_30s_422`.
  - `repo_community.rs`'s `gallery_configs_load` now includes `chaos/`.
- [ ] **Step 2:** Run `cargo test -p mockingbird --test config --test repo_community && cargo test -p mb-admin --test scenarios`. Expected: FAIL.
- [ ] **Step 3:** Implement, and add commented examples of every new key to `docs/examples/mockingbird.yaml`.
- [ ] **Step 4:** Run `cargo test --workspace --all-features`. Expected: PASS.
- [ ] **Step 5:** Commit: `feat(config): role, bus, limits, PII, TLS, outbound, chaos and protocol validation`.

---

### Task 11: Bus and leader election in `mb-state`

**Wave 2. Depends on Task 4.**

**Files:**
- Create: `crates/mb-state/src/{bus.rs,leader.rs}`, `crates/mb-state/tests/{bus.rs,leader.rs}`
- Modify: `crates/mb-state/{Cargo.toml (sha2, uuid, futures),src/lib.rs}`

**Interfaces:**
- Produces:

```rust
// bus.rs
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)] #[serde(tag = "type", rename_all = "snake_case")]
pub enum BusEvent {
    Mock { workspace: String, name: String }, Profile { workspace: String, name: String }, Workspace { name: String },
    State { workspace: String, mock: String, session: String, resource_type: String, id: String, deleted: bool },
    RequestLogged { id: i64 },
    /// Delivered locally after a reconnect, or sent instead of an event over MAX_BUS_PAYLOAD: receivers reload everything.
    Resync,
}
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)] pub struct BusEnvelope { pub origin: String, pub event: BusEvent }
pub const MAX_BUS_PAYLOAD: usize = 7900;
pub const PG_BUS_CHANNEL: &str = "mockingbird_bus";
pub const REDIS_BUS_CHANNEL: &str = "mockingbird:bus";
#[async_trait] pub trait Bus: Send + Sync {
    async fn publish(&self, event: BusEvent) -> Result<(), StateError>;      // never delivered back to this node
    fn subscribe(&self) -> broadcast::Receiver<BusEvent>;                    // events from other nodes, plus local Resync
    fn health(&self) -> Result<(), String>;
    fn node_id(&self) -> &str;
    fn is_local(&self) -> bool;
}
pub struct LocalBus; impl LocalBus { pub fn new(node_id: String) -> Arc<Self>; }       // publish = no-op; health Ok
pub struct MemoryBusHub; impl MemoryBusHub { pub fn new() -> Arc<Self>; pub fn join(self: &Arc<Self>, node_id: String) -> Arc<dyn Bus>; } // tests: in-process multi-node; is_local() == false
pub struct PgBus;    impl PgBus    { pub async fn connect(url: &str, node_id: String) -> Result<Arc<Self>, StateError>; }
pub struct RedisBus; impl RedisBus { pub async fn connect(url: &str, node_id: String) -> Result<Arc<Self>, StateError>; }
// leader.rs
pub const LEADER_LEASE_TTL: Duration = Duration::from_secs(15);
pub const LEADER_RENEW_INTERVAL: Duration = Duration::from_secs(5);
pub enum LeaderBackend { Always, Postgres(String), Redis(String) }
pub struct Leadership { /* watch::Receiver<bool>, task handle aborted on drop, releases on drop */ }
impl Leadership {
    /// Makes the first attempt before returning, so a lone node is leader on return.
    pub async fn start(backend: LeaderBackend, name: &str, node_id: String) -> Result<Self, StateError>;
    pub fn is_leader(&self) -> bool;
    pub fn watch(&self) -> watch::Receiver<bool>;
}
```

**Rules:**
- **`PgBus`** uses `sqlx::postgres::PgListener` on `PG_BUS_CHANNEL`, and publishes with `SELECT pg_notify($1, $2)` on a pool.
- **`RedisBus`** uses a dedicated pub/sub connection and publishes with `PUBLISH`.
- Both buses:
  - drop envelopes whose `origin == node_id`;
  - on a lost connection, set health to `Err(<reason>)` and reconnect with backoff (1 s doubling to 30 s);
  - after each successful reconnect, send `Resync` to subscribers;
  - publish `Resync` instead of any event whose envelope is over `MAX_BUS_PAYLOAD`;
  - count `mb_bus_events_total` and set `mb_bus_connected`.
- **Postgres leader:**
  - uses a dedicated `PgConnection` (not a pool) and `pg_try_advisory_lock(key)`, with the key from the Global Constraints;
  - while leading, runs `SELECT 1` every `LEADER_RENEW_INTERVAL`, and drops leadership and the connection when it fails;
  - retries every `LEADER_RENEW_INTERVAL`.
- **Redis leader:**
  - acquires with `SET mockingbird:leader:<name> <node_id> NX PX 15000`;
  - renews with a Lua compare-and-`PEXPIRE` every 5 s;
  - releases on drop with a Lua compare-and-`DEL`;
  - a renewal that fails or finds a different holder ends leadership at once.
- `mb_leader` reflects the state.

- [ ] **Step 1: Write failing tests:**
  - `tests/bus.rs`:
    - `memory_hub_delivers_to_others_not_self`;
    - `local_bus_is_silent`;
    - `envelope_round_trip_json`;
    - `oversized_event_becomes_resync` (on the memory hub, with the same size rule);
    - gated `pg_bus_delivers_between_nodes`;
    - gated `pg_bus_reconnect_emits_resync`: terminate the listener backend with `pg_terminate_backend` from another connection, then expect `Resync` within 35 s under real time, and use a short test-only backoff through `PgBus::connect_with_backoff(…, Duration::from_millis(100))`;
    - gated `redis_bus_delivers_between_nodes`;
    - gated `redis_bus_reconnect_emits_resync`: `CLIENT KILL TYPE pubsub`.
  - `tests/leader.rs`:
    - `always_is_leader`;
    - gated `pg_leader_single_holder`: two `Leadership`s, exactly one leader, and dropping it promotes the other within 2 × renew;
    - gated `pg_leader_lost_on_connection_drop`;
    - gated `redis_leader_single_holder`;
    - gated `redis_leader_expires_and_fails_over`: delete the key out-of-band, the holder notices on renew, the other acquires.
  - The test-only backoff and renew intervals come from `Leadership::start_with(backend, name, node_id, LeaderTimings)` and `PgBus::connect_with_backoff`. Both are `pub` and `#[doc(hidden)]`.
- [ ] **Step 2:** Run `scripts/test-redis.sh -p mb-state` (inside `scripts/test-postgres.sh` when both are available). Expected: FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Run the same command. Expected: PASS, with gated tests reporting `skipped:` where their server is missing.
- [ ] **Step 5:** Commit: `feat(state): cross-node bus and leader election (Postgres, Redis)`.

---

### Task 12: Engine guards

**Wave 2. Depends on Task 3.**

**Files:**
- Create: `crates/mb-engine/src/guards.rs`, `crates/mb-engine/tests/guards.rs`
- Modify: `crates/mb-engine/src/{lib.rs,stream.rs,message.rs}`

**Interfaces:**
- Produces:

```rust
pub struct GenerationLimiter { /* Arc<Semaphore>, max */ }
impl GenerationLimiter { pub fn new(max: usize) -> Arc<Self>; pub fn in_use(&self) -> usize; pub fn max(&self) -> usize; }
pub trait TokenBudget: Send + Sync {
    /// Checked before every LLM call; an implementation records its own metric when it returns true.
    fn exhausted(&self) -> bool;
    fn spend(&self, tokens: u64);
}
#[derive(Clone, Default)]
pub struct EngineGuards {
    pub limiter: Option<Arc<GenerationLimiter>>, pub budget: Option<Arc<dyn TokenBudget>>,
    pub scrubber: Option<Arc<crate::pii::Scrubber>>, pub max_response_bytes: Option<usize>,
}
impl Engine { pub fn with_guards(self, guards: EngineGuards) -> Self; pub fn guards(&self) -> &EngineGuards; }
```

**Rules** (every LLM call path: `generate`, the repair call, `generate_stream`, `generate_message`):
1. `budget.exhausted()` → skip the LLM and use the faker (`Source::Fallback`), logging `token budget exhausted`.
2. Acquire a limiter permit with `tokio::time::timeout(remaining call timeout)`. On timeout, use the faker and increment `mb_generation_queue_timeouts_total`.
   - The permit is held for the primary and repair calls together.
   - For a stream, it is held while the stream's LLM request is open.
   - `mb_generation_in_flight` tracks the permits held.
3. After the call: `budget.spend(prompt + completion tokens)`.
4. Apply `scrubber.scrub_value` to the parsed JSON (a body, a stream item or a message) **before** validation, and add the counts to `mb_pii_replacements_total{pattern}`.
5. A serialized LLM body larger than `max_response_bytes` counts as a failed generation: fall back to the faker without a repair, with reason `response of N bytes exceeds the M-byte cap`.

- [ ] **Step 1: Write failing tests** (`tests/guards.rs`; `mb_llm::scripted` provider, paused time where useful):
  - `saturated_limiter_falls_back_within_timeout`: limiter max 1, one call holds a slow scripted response; a second call with a 200 ms timeout returns `Fallback` in under 300 ms.
  - `limiter_releases_permits`: `in_use()` returns to 0 after success, error and stream drop.
  - `exhausted_budget_skips_llm`: the scripted provider records zero calls and the source is `Fallback`.
  - `budget_receives_spent_tokens`.
  - `scrubber_runs_before_validation`: the scripted body contains `jane@corp.com`, and the response has `@example.com`.
  - `scrubbed_output_still_validates`: a schema with `format: email` and `pattern: "^\\d{3}-\\d{2}-\\d{4}$"` passes, with source `Llm`.
  - `scrubber_applies_to_stream_items_and_messages`.
  - `oversized_llm_body_falls_back_without_repair`: the provider is called exactly once.
- [ ] **Step 2:** Run `cargo test -p mb-engine --test guards`. Expected: FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Run `cargo test -p mb-engine`. Expected: PASS.
- [ ] **Step 5:** Commit: `feat(engine): generation semaphore, token budget, PII scrubbing and response cap`.

---

### Task 13: Console chaos and limits editor

**Wave 2. Depends on Tasks 1 and 8.**

**Files:**
- Create: `web/src/pages/mocks/ChaosLimits.tsx`, `web/src/test/chaos.test.tsx`
- Modify: `web/src/pages/mocks/{Overview.tsx,ScenarioEditor.tsx}`, `web/src/api/queries.ts` (types only)

**Behaviour:**
- **Overview section "Chaos and limits"** (`aria-labelledby` heading). Fields:
  - Error rate (%) 0–100;
  - Latency jitter min/max (ms) 0–30000, with min ≤ max;
  - Dropped streams (%) 0–100;
  - Rate limit (requests/s, burst);
  - Daily token budget;
  - Max request/response size (KiB).

  Saving sends `PATCH` with `chaos` (null when every chaos field is empty or 0) and `limits` (the whole object).
  - Client-side validation mirrors Task 1's ranges. A server 422 is shown with `ProblemAlert`.
  - The section is read-only with the reason for file mocks and viewers.
  - A hint line: `Callers can bypass chaos with X-Mockingbird-Chaos: off when header overrides are enabled.`
- **ScenarioEditor:** an optional "Chaos override" fieldset with the same three chaos fields. Leaving it empty removes `rules.chaos`.

- [ ] **Step 1: Write failing Vitest tests:**
  - `saves chaos and limits as patch`;
  - `percent fields convert to rates`;
  - `jitter min above max blocks save`;
  - `clearing chaos sends null`;
  - `422 is shown`;
  - `file mock section is read only`;
  - `scenario chaos override round trips`.
- [ ] **Step 2:** Run `npm --prefix crates/mb-console/web test`. Expected: FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Run typecheck, lint, test, build and `check:api` (as in Task 8). Expected: pass.
- [ ] **Step 5:** Commit: `feat(console): chaos and limits settings`.

---

### Task 14: Admission limits and guard wiring

**Wave 3. Depends on Tasks 2, 10 and 12.**

**Files:**
- Create: `crates/mb-protocols/src/limits.rs`, `crates/mockingbird/src/budget.rs`, `crates/mockingbird/tests/limits.rs`
- Modify:
  - `crates/mb-protocols/src/{lib.rs,call.rs,http_rest/handler.rs,graphql/mod.rs,soap/mod.rs,grpc/server.rs,ws.rs}`;
  - `crates/mockingbird/src/{runtime.rs,config.rs,ops.rs,lib.rs}`.

**Interfaces:**
- Produces:

```rust
// mb-protocols limits.rs
pub struct RateLimiter { /* token bucket under a Mutex: tokens: f64, last: Instant */ }
impl RateLimiter { pub fn new(requests_per_second: f64, burst: u32) -> Self; pub fn try_acquire(&self) -> Result<(), Duration /* retry after */>; }
#[derive(Clone)] pub struct RuntimeLimits { pub rate: Option<Arc<RateLimiter>>, pub max_request_bytes: usize }
impl Default for RuntimeLimits { /* None, 1 MiB */ }
// MockRuntime gains: pub limits: RuntimeLimits
// mockingbird budget.rs
pub const BUDGET_FLUSH_INTERVAL: Duration = Duration::from_secs(5);
pub struct StoreTokenBudget { /* store, mock_id, label, limit, day, unflushed, known_total */ }
impl StoreTokenBudget { pub fn spawn(store: Arc<dyn ControlStore>, mock: &Mock, limit: u64) -> Arc<Self>; pub fn is_exhausted(&self) -> bool; }
impl mb_engine::TokenBudget for StoreTokenBudget { … }
// RuntimeDeps gains: pub limiter: Arc<GenerationLimiter>, pub scrubber: Option<Arc<Scrubber>>, pub limits: LimitsConfig
// MockRegistry gains: pub fn budget_issues(&self) -> Vec<String>   // "mock <ws/m>: daily token budget exhausted"
```

**Rules:**
- **Admission order** (spec §7, step 2): tenants → quota → **rate limit** → data-plane auth. Apply it in `call::plan` and `call::admit` (GraphQL whole-request admission).
  - A rate-limited call is `Rejection::new(429, "Too Many Requests", "mock <m> allows <rps> requests/s (burst <b>)").with_retry_after(d)` and increments `mb_rate_limited_total{mock}`.
  - When the operation declares a 429 variant, REST/SOAP/GraphQL answer through the engine with `forced_status = 429` (the contract's shape) plus `Retry-After`. Otherwise they answer with the existing rejection rendering. gRPC answers `RESOURCE_EXHAUSTED`.
- **Request size cap:** `max_request_bytes` = the mock's limit, else `limits.max_request_bytes`.
  - REST checks `Content-Length` first, then the collected length → 413 problem `Payload Too Large`.
  - GraphQL `read_body` and SOAP use the runtime value instead of their constants → 413 (a SOAP Fault with HTTP 413).
  - gRPC decodes with `min(grpc_max_message_bytes, max_request_bytes)`.
  - WebSocket operations use it as `max_message_bytes`.
  - Every 413 is reported to the recorder.
- **`runtime.rs` (P6's `build_engine`)** attaches `EngineGuards`:
  - the shared `limiter`, built once in `build_app_with` from `limits.max_concurrent_generations`;
  - the shared `scrubber` from `cfg.scrubber_config()`;
  - `budget` = `StoreTokenBudget` when the mock has `daily_token_budget`;
  - `max_response_bytes` = the mock's value, else the global one.
- **`StoreTokenBudget`:**
  - `spend` adds to `unflushed`;
  - a task flushes every `BUDGET_FLUSH_INTERVAL` with `add_token_usage(mock_id, today_utc, unflushed.swap(0))` and stores the returned total;
  - `exhausted()` = `known_total + unflushed >= limit`, and resets at UTC midnight. When it returns true it increments `mb_token_budget_exhausted_total{mock}`.
  - `mb_token_budget_used{mock}` is set on each flush.
  - The task ends when the runtime is dropped (it holds a `Weak`).
- `/readyz` lists `registry.budget_issues()` as `degraded` reasons.

- [ ] **Step 1: Write failing tests:**
  - Unit tests in `limits.rs`: `bucket_allows_burst_then_refills`, `retry_after_matches_deficit` (paused time).
  - `crates/mockingbird/tests/limits.rs` (in-process `build_app`):
    - `rate_limited_call_is_429_with_retry_after`: rps 1, burst 2, three quick calls → the third is 429 with `Retry-After: 1`.
    - `rate_limit_uses_declared_429_shape`: a fixture operation declaring 429 → body valid against it.
    - `oversized_request_is_413_and_recorded`: REST, GraphQL and SOAP; the request log has a 413 row for each.
    - `grpc_oversized_message_resource_exhausted`.
    - `budget_exhausted_serves_fallback_and_degrades_readyz`: wiremock LLM reporting `usage.total_tokens` 600, budget 1000 → the second call still `llm`; after a flush the third is `fallback` and `/readyz` is `degraded` with the reason.
    - `budget_shared_through_store`: two `StoreTokenBudget`s on one SQLite file see each other's spend after a flush.
    - `concurrency_limit_from_config`: `max_concurrent_generations: 1` with a slow wiremock → concurrent calls include a `fallback`.
    - `pii_enabled_scrubs_responses`: wiremock returns `jane@corp.com`; the response has `@example.com` and the metric increments.
- [ ] **Step 2:** Run `cargo test -p mb-protocols limits && cargo test -p mockingbird --test limits`. Expected: FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Run `cargo test --workspace --all-features`. Expected: PASS.
- [ ] **Step 5:** Commit: `feat(limits): per-mock rate limits, request caps, daily token budgets and engine guards`.

---

### Task 15: HA change propagation

**Wave 3. Depends on Tasks 10 and 11.**

**Files:**
- Create: `crates/mockingbird/src/ha.rs`, `crates/mockingbird/tests/ha.rs` (the in-process part; Task 23 adds the gated cross-process part)
- Modify:
  - `crates/mb-state/src/{lib.rs,memory.rs,sql.rs,redis.rs}`: `StateChanged` gains `pub origin: ChangeOrigin`.
  - `crates/mb-protocols/src/events/triggers.rs`: broker-channel state triggers ignore `Remote`.
  - `crates/mockingbird/src/{config.rs,registry.rs,recorder.rs,ops.rs,lib.rs}`.

**Interfaces:**
- Produces:

```rust
// mb-state
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)] pub enum ChangeOrigin { #[default] Local, Remote }
// mockingbird ha.rs
#[derive(Clone, Debug, PartialEq)] pub enum HubEvent { Change(mb_store::ControlChange), Resync }
pub struct Ha { pub bus: Arc<dyn Bus>, pub node_id: String, hub: broadcast::Sender<HubEvent> }
impl Ha {
    /// Connects the bus for cfg.bus_target(); spawns the bridges below.
    pub async fn start(cfg: &FileConfig, store: Arc<dyn ControlStore>, node_id: String) -> anyhow::Result<Arc<Self>>;
    pub async fn start_with_bus(bus: Arc<dyn Bus>, store: Arc<dyn ControlStore>) -> Arc<Self>;   // tests (MemoryBusHub)
    pub fn changes(&self) -> broadcast::Receiver<HubEvent>;
    pub fn shared_state(&self, inner: Arc<dyn StateStore>) -> Arc<dyn StateStore>;            // BusStateStore
    pub fn leader_backend(cfg: &FileConfig) -> mb_state::LeaderBackend;                        // from bus_target
}
// MockRegistry::spawn_watcher(self, deps, changes: broadcast::Receiver<HubEvent>)  — Resync and Lagged → reload_all
// StoreRecorder gains: pub fn with_bus(self: Arc<Self>, ha: Arc<Ha>) ; live() merges remote RequestLogged
// RequestRecord gains: pub fn from_entry(e: &RequestLogEntry, workspace: &str, mock: &str) -> Self  (in mockingbird::recorder, as a free fn)
```

**Bridges:**
1. `store.subscribe()` → hub `Change` and `bus.publish` (Mock/Profile/Workspace).
2. Bus Mock/Profile/Workspace → hub `Change`. Bus `Resync` → hub `Resync`.
3. **`BusStateStore`** wraps the state store and delegates every method.
   - `subscribe()` returns a merged broadcast: inner local changes (`origin: Local`, each also published as `BusEvent::State`) plus bus `State` events (`origin: Remote`).
   - Both the registry's runtimes and `AdminState` use the wrapped store.
4. **Recorder:** after `append_request_log` returns an id, the recorder publishes `RequestLogged { id }` (skipped when `bus.is_local()`).
   - A receiver looks up the row with `get_request_log(id)` and the mock's workspace and name through a `mock_id → (ws, name)` cache, filled from `list_all_mocks` on a miss. It sends the `RequestRecord` on the live channel.
5. `/readyz` adds `bus: <reason>` (`degraded`) when `bus.health()` is `Err`.

`build_app_with` creates `Ha` before the registry, using one `node_id`: the existing `EventDeps.holder` UUID is reused. Single-node behaviour is unchanged, because the local bus publishes nothing.

- [ ] **Step 1: Write failing tests** (`tests/ha.rs`; two `build_app_with` nodes on one SQLite **file** with role `all`, sharing a `MemoryBusHub`; the test hook is `config::build_app_with_bus(args, llm, ext, bus)`, which is `#[doc(hidden)] pub`):
  - `mock_created_on_a_is_served_by_b`: within 2 s.
  - `mock_deleted_on_a_stops_on_b`.
  - `state_written_on_b_reaches_change_feeds_on_a`: an SSE `change_feed` subscriber on A sees B's upsert.
  - `remote_state_change_does_not_fire_broker_trigger`: the memory broker of A receives exactly one message when B's write triggers `on_state_change`.
  - `request_logged_on_b_appears_in_live_feed_on_a`.
  - `resync_reloads_all_mocks`: inject `Resync` and the registry reloads (a counter hook or a changed mock is visible).
  - `bus_down_degrades_readyz`: a test bus with a settable health.
  - `single_node_unchanged`: the existing `e2e.rs` keeps passing.
- [ ] **Step 2:** Run `cargo test -p mockingbird --test ha`. Expected: FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Run `cargo test --workspace --all-features`. Expected: PASS.
- [ ] **Step 5:** Commit: `feat(ha): propagate control, state and request-log changes across nodes`.

---

### Task 16: Deployment: Helm chart, Compose profiles and the Compose e2e flow

**Wave 3. Depends on Task 10** (the flag and key names are fixed by this plan).

**Files:**
- Create:
  - `deploy/helm/mockingbird/{Chart.yaml,values.yaml,values.schema.json,README.md,.helmignore,files/default-config.yaml}`;
  - `deploy/helm/mockingbird/templates/{_helpers.tpl,configmap.yaml,serviceaccount.yaml,deployment-all.yaml,deployment-control.yaml,deployment-data.yaml,service.yaml,service-admin.yaml,ingress.yaml,ingress-grpc.yaml,hpa.yaml,pdb.yaml,networkpolicy.yaml,servicemonitor.yaml,NOTES.txt}`;
  - `deploy/helm/mockingbird/ci/{all-in-one-values.yaml,split-values.yaml}`, `deploy/helm/mockingbird/examples/rauthy-values.yaml`;
  - `deploy/compose/mockingbird-ha.yaml`, `scripts/check-helm.sh`;
  - `crates/mockingbird/tests/deploy_artifacts.rs`, `crates/mb-console/web/e2e-compose/flow.spec.ts` (run by the Rauthy Playwright config).
- Modify: `deploy/compose/{docker-compose.yml,README.md}`, `crates/mb-console/web/playwright.rauthy.config.ts` (testMatch includes `flow.spec.ts`)

**Chart contract:**
- `Chart.yaml`: `apiVersion: v2`, `name: mockingbird`, `version: 0.1.0`, `appVersion: "0.1.0"`, `kubeVersion: ">=1.27.0-0"`. There are no subchart dependencies: Rauthy and any other OIDC issuer are external, and `examples/rauthy-values.yaml` shows the wiring.
- **`values.yaml` top-level keys:**
  - `image {repository: ghcr.io/rjs-git/mockingbird, tag: "" (→ appVersion), flavor: full|slim (slim appends -slim), pullPolicy}`, `imagePullSecrets`;
  - `mode: allInOne | split`;
  - `config: {}`: deep-merged with `mergeOverwrite` over `files/default-config.yaml` (`server.host: 0.0.0.0`; `auth.mode: none` with `server.insecure_no_auth: true` is **not** set, so users must configure `auth`; the NOTES warn when `auth.mode` is unset). It is rendered as `mockingbird.yaml` in a ConfigMap, and its `checksum/config` pod annotation rolls the pods on change.
  - `storage`, `state` and `bus`, each `{url, existingSecret, existingSecretKey}`, map to `MB_STORAGE_URL`, `MB_STATE_URL` and `MB_BUS_URL`. In `split` mode `storage.url` or `storage.existingSecret` is `required`.
  - `llm {existingSecret, apiKeyKey: api-key}` → `MB_LLM_API_KEY`;
  - `env`, `envFrom`;
  - `allInOne`, `control` and `data` blocks: `{replicas, resources, nodeSelector, tolerations, affinity, podAnnotations, topologySpreadConstraints}`. `data.autoscaling {enabled: true, minReplicas: 2, maxReplicas: 10, targetCPUUtilizationPercentage: 70}`.
  - `service {type: ClusterIP, ports {http: 8080, grpc: 50051, admin: 9090}}`;
  - `ingress` (HTTP to the data service), `grpcIngress` (annotation `nginx.ingress.kubernetes.io/backend-protocol: GRPC`), `adminIngress` (console and Admin API to the control or all-in-one service);
  - `podDisruptionBudget {enabled: true, minAvailable: 1}`;
  - `networkPolicy {enabled: false, ingressFrom: [], egress: allow-all unless restricted}`;
  - `serviceMonitor {enabled: false, interval: 30s, labels: {}}` (path `/metrics` on `admin`);
  - `serviceAccount {create: true}`;
  - `securityContext`: `runAsNonRoot`, uid 65532, `readOnlyRootFilesystem`, `allowPrivilegeEscalation: false`, capabilities drop `ALL`, `seccompProfile: RuntimeDefault`.
- **Containers:** args `["serve", "-c", "/etc/mockingbird/mockingbird.yaml", "--role", "<all|control|data>"]`.
  - Liveness `GET /healthz` and readiness `GET /readyz` on port `admin`.
  - `terminationGracePeriodSeconds: 30` and `preStop: sleep 5`.
  - `/tmp` is an `emptyDir`.
- **Rendering:** `allInOne` renders one Deployment (role `all`). `split` renders the control and data Deployments, and the HPA targets data.

**Compose:**
- Add `redis` (redis:7-alpine, profile `ha`), `kafka`, `rabbitmq` and `mosquitto` under the profiles `kafka`, `amqp` and `mqtt`, plus `mockingbird-control` and `mockingbird-data` under profile `ha`. They use `mockingbird-ha.yaml` with Postgres storage, Redis state and Redis bus.
- The default (no profile) stack stays exactly as it is.
- The README documents `docker compose --profile ha up`.

**Compose e2e flow** (`flow.spec.ts`, spec §15; it runs in CI's existing `e2e` job):
1. Log in through Rauthy.
2. Create the mock `flow`.
3. Import `fixtures/smart/petstore.har` (HAR is deterministic, so no LLM is needed).
4. Approve, then activate.
5. Call `GET /flow/api/pets/1` with `X-Mockingbird-Session: e2e`, then POST a pet.
6. The Sessions tab shows session `e2e` with ≥ 1 resource.
7. Reset it, and the session is gone.

**`scripts/check-helm.sh`:**
- installs helm `v3.19.0` and kubeconform `v0.7.0` into `.tools/` when missing (prints `skipped: cannot download helm` and exits 0 when offline);
- runs `helm lint --strict` with each `ci/*.yaml`;
- runs `helm template … | kubeconform -strict -summary -kubernetes-version 1.30.0 -ignore-missing-schemas`;
- fails when `mode: split` without storage renders successfully.

- [ ] **Step 1: Write the failing test** `deploy_artifacts.rs`:
  - `chart_app_version_matches_workspace`.
  - `chart_default_config_is_valid`: deep-merge `files/default-config.yaml` with `values.yaml`'s `config` (later wins; the same rule as the chart's `mergeOverwrite`), write the result to a temp file and load it with `file_config::load`. Do the same for each `ci/*.yaml`'s `config`.
  - `values_schema_is_json`.
  - `compose_profiles`: the services `redis`, `kafka`, `rabbitmq`, `mosquitto`, `mockingbird-control` and `mockingbird-data` exist with the expected profiles, and the default services are unchanged.
  - `compose_ha_config_is_valid`: load `mockingbird-ha.yaml` and assert `server.role` is absent there (the role comes per service as `--role`).
- [ ] **Step 2:** Run `cargo test -p mockingbird --test deploy_artifacts`. Expected: FAIL.
- [ ] **Step 3:** Write the chart, the Compose changes, the spec and the script.
- [ ] **Step 4:** Run `cargo test -p mockingbird --test deploy_artifacts && scripts/check-helm.sh && npx --prefix crates/mb-console/web playwright test -c crates/mb-console/web/playwright.rauthy.config.ts --list`. Expected: PASS. Helm may print `skipped`; the spec must be listed.
- [ ] **Step 5:** Commit: `feat(deploy): Helm chart, Compose HA and broker profiles, Compose e2e flow`.

---

### Task 17: P4 carry-overs: GraphQL/WS caps, stream metering, rejection logging and `?wsdl`

**Wave 4. Depends on Task 14.**

**Files:**
- Modify: `crates/mb-protocols/src/{graphql/mod.rs,graphql/resolve.rs,graphql/subscription.rs,grpc/service.rs,soap/mod.rs,soap/envelope.rs,call.rs}`, `crates/mb-protocols/tests/{graphql.rs,grpc.rs,soap.rs}`

**Rules:**
- **Root-field cap:** before execution, count the root fields of the selected operation, with fragments on the root type expanded. Over `graphql_max_root_fields` → HTTP 400 with `errors: [{message: "too many root fields: N (maximum M)", extensions: {code: "TOO_MANY_ROOT_FIELDS"}}]`. It is recorded and no engine call is made.
- **Subscriptions** (both subprotocols):
  - A connection's `(graphql_max_subscriptions_per_connection + 1)`-th concurrent subscription gets an `error` message with `too many subscriptions on this connection (maximum N)`, and the connection stays open.
  - Once `graphql_max_connections_per_mock` sockets are open on a mock, the next upgrade is answered 503 problem `too many GraphQL WebSocket connections`, with `Retry-After: 1`. It is counted by a per-runtime `AtomicUsize` guard released on close.
  - Both refusals are recorded.
- **Client and bidi gRPC streaming:** each engine call driven by an inbound message runs `ext.quota.admit` and the mock's rate limiter, and records one `UsageEvent` with that item's tokens.
  - A refusal ends the stream with `RESOURCE_EXHAUSTED`, carrying the rejection detail.
  - Earlier items have already been sent and metered.
- **Rejection logging:** every response the GraphQL adapter sends before execution is reported through `call::report` with its status (400 invalid request, 405, 413, 401 in every data-plane auth mode, root-field cap, connection cap). Write the test first: the current code may already cover some of these (`report_refusal`, `admit_request`); fix only what fails.
- **`?wsdl` base URL:**
  - Scheme: `https` when `tls_terminated`; else, with `trust_forwarded_headers`, the first `X-Forwarded-Proto` value or the RFC 7239 `Forwarded: proto=`; else `http`.
  - Host: with `trust_forwarded_headers`, the first `X-Forwarded-Host` value or `Forwarded: host=`; else `Host`.
  - The mount prefix is unchanged. Header values are validated (a host with `/` or whitespace is ignored).
- **P4 ledger minors** (SOAP namespace count, gRPC duplicate-symbol union, NCName ranges): fix them only if the P4 review ledger is available to the executor and the fix stays within these files.

- [ ] **Step 1: Write failing tests:**
  - `graphql.rs`:
    - `root_field_cap_rejects_before_engine` (21 aliases of one field; the scripted provider records zero calls; the request log has a 400);
    - `fragment_spread_counts_toward_cap`;
    - `subscription_cap_per_connection` (both subprotocols);
    - `connection_cap_per_mock_503`;
    - `rejections_are_recorded` (one sub-case each for 400, 405, 413, 401 api_key HTTP, 401 passthrough HTTP, 401 WS);
  - `grpc.rs`:
    - `bidi_stream_meters_each_item` (a counting `UsageSink` sees N events for N inbound messages);
    - `client_stream_admits_each_item`;
    - `bidi_quota_refusal_ends_stream_resource_exhausted`;
  - `soap.rs`:
    - `wsdl_uses_forwarded_headers_when_trusted`;
    - `wsdl_ignores_forwarded_headers_by_default`;
    - `wsdl_forwarded_rfc7239`;
    - `wsdl_https_when_tls_terminated`;
    - `wsdl_rejects_bad_forwarded_host`.
- [ ] **Step 2:** Run `cargo test -p mb-protocols --all-features --test graphql --test grpc --test soap`. Expected: FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Run `cargo test --workspace --all-features`. Expected: PASS.
- [ ] **Step 5:** Commit: `fix(protocols): GraphQL root-field and WebSocket caps, per-item stream metering, rejection logging, forwarded ?wsdl`.

---

### Task 18: Roles, `server.rs` and leader-gated work

**Wave 4. Depends on Task 15.**

**Files:**
- Create: `crates/mockingbird/src/server.rs`, `crates/mockingbird/tests/roles.rs`
- Modify:
  - `crates/mockingbird/src/{main.rs,config.rs,registry.rs,ops.rs,maintenance.rs,lib.rs}`;
  - `crates/mb-protocols/src/events/{runtime.rs,mqtt.rs,mod.rs}`;
  - `crates/mb-state/src/leader.rs` (`LeaderBackend::Manual`, test-only).

**Interfaces:**
- Produces:

```rust
// server.rs
#[derive(Debug)] pub enum ServeError { Usage(String), Other(anyhow::Error) }
/// Validates, binds the role's listeners, serves until SIGINT/SIGTERM, then shuts down gracefully (P1–P5 behaviour).
pub async fn run_with(args: ServeArgs, llm: Option<LlmEnv>, ext: Extensions) -> Result<(), ServeError>;
pub async fn run(args: ServeArgs) -> Result<(), ServeError>;   // run_with(args, Some(LlmEnv::from_env()), Extensions::default())
pub struct Listeners { pub data: Option<TcpListener>, pub admin: Option<TcpListener>, pub grpc: Option<TcpListener> }
/// Which listeners a role binds: (data, admin, grpc).
pub fn binds_for(role: Role) -> (bool, bool, bool);
/// Serves `app` on `listeners` until `stop` turns true, then drains and calls `app.shutdown()`. run_with = bind per binds_for + serve_app.
pub async fn serve_app(app: App, listeners: Listeners, stop: tokio::sync::watch::Receiver<bool>) -> anyhow::Result<()>;
// main.rs: Command::Serve(args) => server::run(args) mapped onto the existing Failure variants
// RuntimeDeps gains: pub mode: ServeMode
#[derive(Clone, Copy, Debug, PartialEq, Eq)] pub enum ServeMode { Full, ControlOnly }
// App gains: pub role: Role, pub leadership: Arc<Leadership>, pub ha: Arc<Ha>
// mb-protocols events
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)] pub enum ChannelMode { #[default] Full, PublishOnly }
// EventSetup gains: pub mode: ChannelMode;  EventDeps gains: pub shared_subscriptions: bool
```

**Role behaviour** (spec §4.3):

| | `all` | `control` | `data` |
|---|---|---|---|
| Data listener and gRPC listener | yes | not bound | yes |
| Admin port | Admin API, console, ops | Admin API, console, ops | ops only (`/healthz`, `/readyz`, `/metrics`) |
| Reconcile, `--watch` | leader only | leader only | never; `--watch` is a usage error |
| Maintenance (session expiry, audit retention) | leader only | leader only | no |
| P6 import workers | yes | yes | no |
| Mock runtimes | `Full` | `ControlOnly`: built for manual publish; channel runtimes `PublishOnly` | `Full` |

**Rules:**
- **`PublishOnly` channel runtimes** start no consumers, no triggers and no WebSocket routes.
  - Manual publish on a broker channel works.
  - Manual publish on a WebSocket channel returns `PublishError::NotPublishable("WebSocket channels are served by data nodes; publish from a data node or run role all")` (409).
- **Leadership** comes from `Ha::leader_backend(cfg)` with lock name `control`. With a local bus it is `Always`.
  - At startup a leader reconciles before the registry loads, as today. A non-leader skips the reconcile.
  - A leader-watch task does three things on every transition to leader: reconcile once (logging errors), start the `--watch` guard if requested, and start `Maintenance`.
  - When leadership is lost, it drops the watch guard and `Maintenance`.
- **Data role:** the authenticator is API keys only: `mb_auth::Authenticator { api_keys, sessions: None, bearer: None }`, so it needs no OIDC discovery. It gates `/metrics` (unless `auth.public_metrics`) and the `/readyz` reasons exactly as before.
- `/readyz` JSON gains `"role"` and `"leader"`; they are always shown.
- `shared_subscriptions = !ha.bus.is_local()`. MQTT consumers then subscribe to `$share/<client_id_prefix>/<topic>`.

- [ ] **Step 1: Write failing tests** (`tests/roles.rs`; nodes run in-process through `config::build_app_with_bus` and `server::serve_app` on ephemeral listeners; SQLite-file control store with role `all` for the leader tests, and gated Postgres for split roles):
  - `data_role_serves_ops_only_on_admin_port`: `/api/v1/mocks` is 404 and `/readyz` is 200.
  - `control_role_binds_no_data_listener`.
  - `data_role_rejects_watch`: usage error.
  - `only_leader_reconciles`: two role-`all` nodes on a `MemoryBusHub` with a test `LeaderBackend` override; the non-leader's reconcile counter stays 0.
  - `leader_gain_starts_watch`: flip the test leader and a config change is applied.
  - `control_publish_only_websocket_channel_409`.
  - `readyz_reports_role_and_leader`.
  - `mqtt_shared_subscription_topic` (unit test in `mqtt.rs`).
  - gated `split_roles_over_postgres`: a control node and a data node on `MB_TEST_POSTGRES_URL` with Postgres state and bus; create a mock on control → data serves it within 3 s.
  - The test leader override is `LeaderBackend::Manual(watch::Receiver<bool>)`, added to `mb-state` behind `#[doc(hidden)]`.
- [ ] **Step 2:** Run `cargo test -p mockingbird --test roles`. Expected: FAIL.
- [ ] **Step 3:** Implement, moving `serve` and `shutdown_signal` from `main.rs` into `server.rs` unchanged in behaviour.
- [ ] **Step 4:** Run `cargo test --workspace --all-features && scripts/test-postgres.sh -p mockingbird --test roles`. Expected: PASS.
- [ ] **Step 5:** Commit: `feat(server): control/data roles with leader-gated reconcile, watch and maintenance`.

---

### Task 19: Chaos

**Wave 5. Depends on Task 17.**

**Files:**
- Create: `crates/mb-protocols/src/chaos.rs`, `crates/mockingbird/tests/chaos.rs`
- Modify:
  - `crates/mb-protocols/src/{lib.rs,call.rs,http_rest/{handler.rs,sse.rs},graphql/{resolve.rs,subscription.rs},grpc/service.rs,soap/mod.rs,ws.rs}`;
  - `crates/mockingbird/src/runtime.rs`: `ChaosSettings` → runtime, plus scenario chaos.

**Interfaces:**
- Produces:

```rust
// mb-protocols cannot depend on mb-store, so it has its own spec type, filled by runtime.rs from mb_store::ChaosSettings:
#[derive(Clone, Debug, Default, PartialEq)] pub struct ChaosSpec { pub error_rate: f64, pub latency_jitter: Option<(Duration, Duration)>, pub drop_stream_rate: f64 }
pub struct ChaosRuntime { pub settings: ChaosSpec, rng: Mutex<rand::rngs::StdRng> }
impl ChaosRuntime { pub fn new(spec: ChaosSpec, seed: Option<u64>) -> Self; }   // seed from env MB_CHAOS_SEED when set
pub struct ChaosDecision { pub inject_error: bool, pub extra_latency: Duration, pub drop_after: Option<usize> /* streams: items before the drop */ }
impl ChaosRuntime { pub fn decide(&self, streaming: bool, planned_items: usize) -> ChaosDecision; }
pub const CHAOS_HEADER: &str = "x-mockingbird-chaos";
// MockRuntime gains: pub chaos: Option<Arc<ChaosRuntime>>;  ScenarioRuntime gains: pub chaos: Option<Arc<ChaosRuntime>>
// CallPlan gains: pub chaos: ChaosDecision
```


**Rules:**
- **Selection:**
  - In `plan`, the chaos source is the applying scenario's `chaos` if it has one, else the mock's.
  - No chaos when `X-Mockingbird-Chaos: off` and `header_overrides` is on. With overrides off, the header is ignored. Any other header value is a 400 override problem: `X-Mockingbird-Chaos must be "off"`.
- **Error injection:** with probability `error_rate`, `forced_status` = the first declared status of 503, 500, the smallest other declared 5xx, or 429.
  - The response is generated through the engine as usual, so it has the contract's shape, and carries `X-Mockingbird-Chaos: error` (or the gRPC metadata / GraphQL extension).
  - When none of those statuses is declared, the protocol-native answer is: REST 503 problem `Service Unavailable` with detail `injected by chaos`; GraphQL a field error with code `UNAVAILABLE`; gRPC `UNAVAILABLE`; SOAP a Server/Receiver Fault with HTTP 503.
  - Header and scenario `forced_status` take precedence: chaos does not inject when a status is already forced.
- **Latency:** `extra_latency` is uniform in `[min, max]`, added to the planned delay, and the total is capped at 30 s.
- **Stream drops:** with probability `drop_stream_rate`, `drop_after` is uniform in `0..planned_items`. After that many items:
  - SSE: the body stream ends with an error, which aborts the connection without a final event.
  - gRPC server/bidi: `Status::unavailable("stream dropped by chaos")`.
  - GraphQL subscription: an `error` message with code `UNAVAILABLE` for that subscription.
  - WebSocket operation: close code 1011.
- `mb_chaos_injected_total{mock,kind}` counts `error`, `latency` and `drop`.

- [ ] **Step 1: Write failing tests:**
  - Unit tests in `chaos.rs`: `zero_rate_never_injects` (10 000 decisions), `full_rate_always_injects`, `seeded_half_rate_is_deterministic_and_near_half`, `jitter_within_bounds`.
  - `crates/mockingbird/tests/chaos.rs` (in-process, offline faker, `MB_CHAOS_SEED` not needed at rates 0 and 1):
    - `error_uses_declared_variant`: REST op declaring 503 → 503, body valid against its schema, header `X-Mockingbird-Chaos: error`.
    - `undeclared_error_is_protocol_native`: REST, GraphQL, gRPC and SOAP.
    - `forced_status_header_wins_over_chaos`.
    - `header_off_bypasses_only_when_overrides_enabled`.
    - `scenario_chaos_replaces_mock_chaos`: a mock rate of 1 with a scenario rate of 0 → no injection under the scenario.
    - `latency_jitter_applied`: paused time or a measured lower bound.
    - `sse_stream_dropped_early`: rate 1, 5 planned items → fewer than 5 events and no final event.
    - `grpc_server_stream_dropped_unavailable`.
    - `graphql_subscription_dropped_error`.
    - `chaos_metric_counts_kinds`.
- [ ] **Step 2:** Run `cargo test -p mb-protocols chaos && cargo test -p mockingbird --test chaos`. Expected: FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Run `cargo test --workspace --all-features`. Expected: PASS.
- [ ] **Step 5:** Commit: `feat(chaos): error injection, latency jitter and stream drops per mock and scenario`.

---

### Task 20: TLS termination and outbound CA/proxy

**Wave 5. Depends on Task 18.**

**Files:**
- Create: `crates/mockingbird/src/tls.rs`, `crates/mockingbird/tests/tls.rs`, `fixtures/tls/{README.md,ca.pem,server.pem,server-key.pem}`. The fixtures are test-only certificates for `localhost` and `127.0.0.1`, valid for 100 years. They are generated once with `rcgen` in a throwaway program, and the README records how.
- Modify:
  - `crates/mockingbird/src/{server.rs,config.rs}`;
  - `crates/mb-llm/src/{lib.rs,openai.rs}`, `crates/mb-auth/src/http.rs`, `crates/mb-admin/src/webhook.rs` (client construction only);
  - `Cargo.toml` (`axum-server`, `rustls`).

**Interfaces:**
- Produces:

```rust
// mb-llm
#[derive(Clone, Debug, Default)] pub struct HttpOptions { pub extra_roots_pem: Vec<u8>, pub proxy: Option<String>, pub no_proxy: Vec<String> }
pub fn http_client(opts: &HttpOptions, timeout: Option<Duration>) -> Result<reqwest::Client, LlmError>;   // add_root_certificate per PEM cert, Proxy::all + NoProxy
// OpenAiConfig gains: pub http: HttpOptions
// mb-auth (gains an `mb-llm` dependency for the type): pub fn http_client_with(opts: &mb_llm::HttpOptions) -> reqwest::Client; http_client() = http_client_with(&Default::default())
// mockingbird tls.rs
pub async fn load_rustls(cfg: &TlsConfig) -> anyhow::Result<axum_server::tls_rustls::RustlsConfig>;  // ServerConfig::builder_with_provider(aws_lc_rs) ; ALPN h2 + http/1.1
pub fn spawn_reloader(cfg: TlsConfig, rustls: RustlsConfig) -> JoinHandle<()>;   // every 60 s: reload when either file's mtime changed; a bad file logs an error and keeps the old certs
```

**Rules:**
- With `server.tls`, all three listeners (data, admin, gRPC) serve TLS through `axum_server::bind_rustls(...).handle(handle)`. The graceful shutdown path is the same: `Handle::graceful_shutdown(Some(GRACEFUL_SHUTDOWN_TIMEOUT))`.
- `auth.external_url` must be `https://` when TLS is on (a startup error otherwise).
- `outbound.*` applies to:
  - every LLM provider client;
  - OIDC discovery/JWKS;
  - the audit webhook;
  - P6's URL import fetcher (CA bundle only: the SSRF fetcher never uses a proxy, as P6 decided).
- The CA file is read at startup. A bad PEM is an error naming `outbound.ca_file`.

- [ ] **Step 1: Write failing tests** (`tests/tls.rs`):
  - `serves_data_admin_and_grpc_over_tls`: a reqwest client with `fixtures/tls/ca.pem` gets `/__mockingbird/healthz` and `/healthz`; a tonic health check over TLS succeeds.
  - `plain_http_refused_when_tls_on`.
  - `cert_reload_on_change`: copy the certs to a temp dir, start, swap in a second pair, advance the reload, and the new serial is presented.
  - `bad_cert_file_is_startup_error`.
  - `llm_client_trusts_outbound_ca`: wiremock over TLS isn't available, so use an axum-server HTTPS stub with the fixture cert.
  - `llm_client_uses_proxy`: a stub proxy records a CONNECT or absolute-URI request.
  - `no_proxy_bypasses`.
  - `external_url_must_be_https_with_tls`.
- [ ] **Step 2:** Run `cargo test -p mockingbird --test tls`. Expected: FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Run `cargo test --workspace --all-features && cargo deny check`. Expected: PASS (add crate-scoped `deny.toml` exceptions only if a new license appears).
- [ ] **Step 5:** Commit: `feat(net): TLS termination with reload, outbound CA bundle and proxy`.

---

### Task 21: `mb-ext` example binary and semver gate

**Wave 5. Depends on Tasks 2 and 18.**

**Files:**
- Create: `examples/custom-ext/{Cargo.toml,README.md,src/main.rs,tests/ext.rs}`, `scripts/semver-check.sh`
- Modify: `Cargo.toml` (`members = ["crates/*", "examples/custom-ext"]`)

**Interfaces:**
- Produces:
  - Crate `mockingbird-custom-ext-example`, `publish = false`, depending on `mb-ext`, `mockingbird`, `async-trait`, `tokio`, `clap`, `dashmap`.

```rust
pub struct HeaderTenants;      // TenantResolver: tenant = X-Tenant header (default "anonymous"), workspace "default"
pub struct FixedQuota { pub per_tenant: u64, counts: DashMap<String, u64> }   // QuotaGuard: Rejection::new(429, "Quota exceeded", …).with_retry_after(60s) after per_tenant calls
pub fn extensions(per_tenant: u64) -> mb_ext::Extensions;   // Extensions::default().with_tenants(..).with_quota(..)
// main: parses mockingbird::config::ServeArgs and calls mockingbird::server::run_with(args, Some(LlmEnv::from_env()), extensions(100))
```

- `scripts/semver-check.sh`:
  - finds the last tag with `git describe --tags --abbrev=0 --match 'v*'`;
  - with none, prints `skipped: no release tag yet` and exits 0;
  - otherwise runs `cargo semver-checks check-release -p mb-ext --baseline-rev <tag>`.

- [ ] **Step 1: Write failing tests** (`examples/custom-ext/tests/ext.rs`):
  - `quota_rejects_after_limit_per_tenant`: `build_app_with(…, extensions(2))`, offline; tenant `a` gets 200, 200, 429 with `Retry-After: 60`; tenant `b` still gets 200.
  - `missing_header_is_anonymous_tenant`.
- [ ] **Step 2:** Run `cargo test -p mockingbird-custom-ext-example`. Expected: FAIL.
- [ ] **Step 3:** Implement. Run `scripts/semver-check.sh`. Expected: `skipped: no release tag yet`.
- [ ] **Step 4:** Run `cargo test -p mockingbird-custom-ext-example && cargo build -p mockingbird-custom-ext-example`. Expected: PASS.
- [ ] **Step 5:** Commit: `docs(ext): custom extension example binary and semver check script`.

---

### Task 22: Per-stage trace spans and the state-size metric

**Wave 6. Depends on Task 19.**

**Files:**
- Create: `crates/mb-protocols/tests/spans.rs`
- Modify:
  - `crates/mb-protocols/src/{call.rs,http_rest/handler.rs,graphql/resolve.rs,grpc/service.rs,soap/mod.rs}`;
  - `crates/mb-engine/src/{lib.rs,stream.rs}`;
  - `crates/mb-state/src/{lib.rs,memory.rs,sql.rs,redis.rs}`;
  - `crates/mockingbird/src/maintenance.rs` (the gauge refresh);
  - `crates/mb-protocols/Cargo.toml` (dev-dep `tracing-subscriber`).

**Rules:**
- Child spans of each adapter's existing request span:
  - `mb.decode`: matching and input validation;
  - `mb.admission`: tenants, quota, rate limit, auth;
  - `mb.state`: reads and writes;
  - `mb.prompt`: `build_messages`;
  - `mb.llm`: each provider call, with attributes `llm.model` and `llm.tokens.prompt`/`llm.tokens.completion`;
  - `mb.validate`;
  - `mb.repair`: only when a repair call runs;
  - `mb.encode`.
- Spans use `tracing::info_span!` and `.instrument(...)` on async work. No span is created per stream item.
- `mb_state_sessions{mock}` gauge: `StateStore` gains `async fn session_count(&self, workspace: &str, mock: &str) -> Result<usize, StateError>`, with a default implementation that calls `sessions(..).len()`. It is refreshed every 60 s for every mounted mock by the maintenance loop, which runs on every node that serves the data plane.

- [ ] **Step 1: Write failing tests** (`spans.rs`, with a capturing `tracing_subscriber::Layer` that records span names and parent names):
  - `rest_call_emits_pipeline_spans`: the scripted provider returns invalid then valid output → all eight names under the request span, `mb.repair` exactly once.
  - `fallback_call_has_no_repair_span_without_second_llm_call`.
  - `grpc_unary_emits_pipeline_spans`.
  - `state_session_count_default_matches_sessions` (`mb-state` conformance, plus Redis when gated).
- [ ] **Step 2:** Run `cargo test -p mb-protocols --all-features --test spans && cargo test -p mb-state`. Expected: FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Run `cargo test --workspace --all-features`. Expected: PASS.
- [ ] **Step 5:** Commit: `feat(observability): per-stage spans and state session gauge`.

---

### Task 23: CI consolidation and the cross-node HA test

**Wave 7. Depends on Tasks 1–22.**

**Files:**
- Create: `crates/mockingbird/tests/ha_cluster.rs`
- Modify: `.github/workflows/ci.yml`

**CI changes (`ci.yml`):**
- **`check` job:**
  - add the service `redis: image: redis:7`, port 6379, health `redis-cli ping`, and `MB_TEST_REDIS_URL: redis://localhost:6379`;
  - add the steps `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features` and `cargo test -p mockingbird-custom-ext-example`.
- **`image` job:** after the build, scan both images with `aquasecurity/trivy-action` (`severity: CRITICAL,HIGH`, `exit-code: 1`, `ignore-unfixed: true`).
- **New jobs:**
  - `helm`: runs `scripts/check-helm.sh`.
  - `semver`: full-depth checkout (`fetch-depth: 0`), then `scripts/semver-check.sh` (the `cargo-semver-checks` install is cached).
  - `msrv`: `cargo +1.97 check --workspace --all-features --locked`. It is kept separate so a `rust-toolchain.toml` bump cannot hide an MSRV regression; the `rust-version` field is the source of truth.
- **`e2e` job:** after the Rauthy login test, `flow.spec.ts` runs through the same config. Then `docker compose --profile ha up -d --wait`, and a curl script checks the HA path:
  1. create a mock through the control node's admin port;
  2. poll the data node's port until it serves it (≤ 30 s);
  3. `docker compose stop mockingbird-control` → the data node still serves;
  4. tear down.
- The `protocol-features` matrix is unchanged.

**`ha_cluster.rs`** (gated on both `MB_TEST_POSTGRES_URL` and `MB_TEST_REDIS_URL`; in-process nodes on ephemeral ports):
- `cluster_end_to_end`: two control nodes and two data nodes (Postgres storage, Redis state and bus).
  - Exactly one control node leads.
  - Create a mock on control 1, and both data nodes serve it.
  - A POST via data 1 is followed by a GET via data 2 in the same session, which sees the resource.
  - The live feed on control 2 shows both calls.
  - Drop the leader node, and the other control node leads within 20 s.
  - Delete the mock, and both data nodes return 404 within 3 s.
- `cluster_redis_bus_outage_degrades_then_recovers`: `CLIENT KILL TYPE pubsub` → the data node's `/readyz` is degraded, then ready again after reconnect, and a `Resync` reloaded its mocks.

- [ ] **Step 1:** Write `ha_cluster.rs`. Run `scripts/test-redis.sh -p mockingbird --test ha_cluster` inside `scripts/test-postgres.sh`. Expected: PASS. A failure here is a defect in Tasks 11, 15 or 18 to fix before continuing.
- [ ] **Step 2:** Update `ci.yml`. Run `cargo test -p mockingbird --test release_workflow` (it parses YAML, which catches syntax errors) and `actionlint` if it is installed.
- [ ] **Step 3:** Run the full local gate:
  - `cargo fmt --all --check && cargo clippy --workspace --all-targets --all-features -- -D warnings && cargo test --workspace --all-features && cargo deny check && RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features`
  - `scripts/test-postgres.sh`
  - `scripts/test-redis.sh`
  - the console scripts and `e2e`
  - `scripts/check-helm.sh`

  Expected: all pass. Report anything that was skipped.
- [ ] **Step 4:** Commit: `ci: redis service, docs, helm, semver, MSRV and image scan gates; cross-node HA test`.

---

### Task 24: Operator docs, configuration reference and README

**Wave 7. Depends on Tasks 1–22** (it can run in parallel with Task 23).

**Files:**
- Create:
  - `docs/{architecture.md,configuration.md,upgrading.md,performance.md}`;
  - `docs/deploy/{local.md,compose.md,kubernetes.md}`, `docs/operations/runbook.md`;
  - `docs/examples/mockingbird.full.yaml`, `crates/mockingbird/tests/docs_reference.rs`.
- Modify:
  - `crates/mockingbird/src/file_config.rs`: add `Serialize` (with `skip_serializing` for the `*_explicit` flags) to `FileConfig` and every nested config type. This is the only allowed edit.
  - `README.md` (slimmed, linking to `docs/`), `CHANGELOG.md` (the P7 bullet filled in), `crates/mb-ext/README.md` (the stability policy).

**Content:**
- **`architecture.md`:** crates and boundaries (§4); roles and what each runs (the Task 18 table); the request flow with chaos, limits and guards; HA (bus events, leader election, what is leader-gated, the per-mock scheduler lease from P5, `Resync`); state backends.
- **`configuration.md`:** every key path in backticks, with type, default, range, env var and flag, grouped by section. Values are taken from the Global Constraints.
- **`deploy/local.md`:** binary, `serve spec.yaml`, offline mode, TLS.
- **`deploy/compose.md`:** the default, `ha`, `kafka`, `amqp` and `mqtt` profiles.
- **`deploy/kubernetes.md`:**
  - chart install for all-in-one and split;
  - external Postgres and Redis;
  - OIDC with Rauthy (`examples/rauthy-values.yaml`);
  - ingress for HTTP, gRPC and admin;
  - ServiceMonitor, NetworkPolicy and HPA;
  - secrets through `existingSecret`, Vault (`vault:` refs) and the Secrets Store CSI driver plus `file:` refs for AWS/GCP/Azure;
  - verifying images with cosign and `gh attestation verify`.
- **`operations/runbook.md`:** one section per signal, giving its metric or `/readyz` reason, its meaning and the action to take:
  - LLM slow or down (fallback ratio, `mb_generation_queue_timeouts_total`);
  - budget exhausted;
  - rate limiting;
  - state store down (degraded);
  - bus down;
  - leader failover (`mb_leader`);
  - broker unavailable;
  - request log dropped;
  - audit webhook failures;
  - certificate rotation;
  - Postgres backup and restore;
  - Redis eviction policy (`noeviction` required for state);
  - rolling upgrades (migrations under advisory lock, control first);
  - scaling data nodes;
  - draining for maintenance.
- **`upgrading.md`:** 0.x upgrade notes; migration `0300` is additive; the `mb-ext` 1.0 changes for extension authors (builders, constructors, `#[non_exhaustive]`).
- **`performance.md`:** the Task 7 bench method and the numbers (machine, concurrency, req/s, p50/p95/p99), and how to rerun them.

**Tests (`docs_reference.rs`):**
- `full_example_loads`: `docs/examples/mockingbird.full.yaml` sets every key, with at least one mock, broker, profile and scenario. It loads through `file_config::load`, which uses `deny_unknown_fields`.
- `every_config_key_is_documented`: serialize `FileConfig::default()`, and the parsed full example, to `serde_json::Value`; collect every key path, with map entries collapsed to `<name>` (`mocks.<name>.chaos.error_rate`). Each path must appear in `docs/configuration.md` as `` `path` ``.
- `docs_links_resolve`: every relative markdown link in `README.md` and `docs/**/*.md` points to an existing file.

- [ ] **Step 1:** Write `docs_reference.rs`. Run `cargo test -p mockingbird --test docs_reference`. Expected: FAIL.
- [ ] **Step 2:** Add `Serialize`, write the docs and the full example, and update the README and CHANGELOG.
- [ ] **Step 3:** Run `cargo test -p mockingbird --test docs_reference --test repo_community && cargo test --workspace --all-features`. Expected: PASS.
- [ ] **Step 4:** Commit: `docs: architecture, configuration reference, deployment guides, runbook, upgrade notes`.
