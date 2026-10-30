# Refinement baseline

Measured on `origin/main` at `b9d4606` (2026-10-29). Every refinement PR reports
its delta against these numbers.

## Local (Apple M-series, 12 cores, macOS, toolchain 1.97, default `dev` profile)

| Measure | Value |
|---|---|
| Clean `cargo test --workspace --all-features --no-run` | **140 s** wall, 1,183 s of unit time over 991 units |
| ... of which workspace units | 165 units, 597 s |
| ... integration-test binaries (`tests/*.rs`) | 113 units, **419 s** |
| ... lib/bin unit-test targets | 17 units, 90 s |
| Slowest single units | `openssl-sys` build script 53 s (rdkafka `ssl-vendored`), `aws-lc-sys` build script 39 s, `mockingbird` lib (test) 29 s |
| Incremental rebuild after touching `mb-engine/src/lib.rs` | **31 s** |
| Incremental rebuild after touching the `mockingbird` binary crate | 2 s |
| Warm `cargo test --workspace --all-features` (all binaries, `--no-fail-fast`) | **221 s** |
| Warm `cargo nextest run --workspace --all-features` | **29 s** (27.5 s test time) |
| Warm `cargo clippy --workspace --all-targets --all-features` | 48 s (after a test build) |
| `target/` after the test build | **14 GB** (`debug/deps` 11 GB; 172 executables ≈ 9.7 GB) |
| `target/` after clippy too | 18 GB |
| Test binaries run by `cargo test` | 144 (incl. doc-test runs) |
| Tests (nextest, no doctests) | **1,627**: 1,626 pass, 1 fails on macOS only (below) |

`cargo test` runs binaries one after another; nextest runs tests from all of them
in parallel. The same tests take 221 s and 29 s respectively.

### Slowest binaries (nextest, summed test time)

| Binary | Tests | Time |
|---|---|---|
| `mockingbird-cli::agent_toolkit` | 19 | 57.9 s |
| `mockingbird-cli::config` | 79 | 20.1 s |
| `mockingbird-cli::client` | 30 | 12.3 s |
| `mockingbird-protocols::events_kafka` | 2 | 10.1 s |
| `mockingbird-cli::observability` | 18 | 9.0 s |
| `mockingbird-auth::oidc` | 19 | 8.4 s |

Slowest tests: `agent_toolkit::mcp_drives_a_background_server` 13.5 s,
`agent_toolkit::start_server_with_the_proxy_returns_its_variables` 11.3 s,
`events_kafka::kafka_unreachable_connect_error` 10.1 s (a connect timeout),
`smart_import::large_html_extraction_does_not_block_the_runtime` 4.8 s,
`observability::unreachable_state_database_fails_fast` 4.2 s,
`config::reload_changing_brokers_is_rejected` 4.1 s. Most other
`agent_toolkit` tests take about 3 s each.

### Known local failure

`mockingbird-cli::tls::llm_client_trusts_outbound_ca` fails on macOS on every run
and passes on Linux CI. The fixture certificate `fixtures/tls/server.pem` is valid
for 100 years, and Apple's platform verifier rejects TLS server certificates with
a validity period over 825 days.

## CI (`ci.yml`, GitHub-hosted runners, warm `rust-cache`)

Last three `main`/PR runs (40907076695, 40905853861, 40781636739):

| Job | Wall time |
|---|---|
| `check` (critical path) | **5 m 38 s, 6 m 01 s, 5 m 22 s** |
| `console` | 3 m 50 s, 2 m 50 s, 2 m 45 s |
| `e2e` | 2 m 13 s, 2 m 31 s (9 m 31 s on a cold image cache) |
| `protocol-features` | 1 m 10 s, 1 m 07 s, 1 m 11 s |
| `deny` | ~40 s |
| `helm`, `semver`, `msrv` | ≤ 25 s |

Steps over 15 s in `check`:

| Step | Seconds |
|---|---|
| `cargo test --workspace --all-features` | 182, 196, 164 |
| Test the custom extension example | 25, 30, 28 |
| `cargo clippy …` | 24, 23, 24 |
| Build the API docs | 18, 22, 20 |
| `rust-cache` restore | 17, 23, 18 |
| Initialize containers | 21, 16, – |

Other notable steps: `protocol-features` loop 34–36 s; `console` `npm run check:api`
34–52 s and Playwright 40–54 s; `e2e` image build 45–57 s warm.

## Lint and code inventory

- Workspace lints deny only `clippy::unwrap_used`; there is no `clippy.toml`.
- 143 `#[allow(clippy::unwrap_used)]`/`expect_used` attributes, mostly in tests.
- In `src`: 15 unconditional `#[allow(dead_code)]` (plus 14 feature-gated `cfg_attr` ones in `mb-protocols`), 6 `#[allow(clippy::too_many_arguments)]`,
  1 `#[allow(unreachable_patterns)]`.
- 160 `sleep(` calls in `tests/`, 42 more in `src` (some in unit tests).

## After

Measured on `origin/main` at `14e399e` (2026-10-30), after Waves 0–7 (#80–#136),
on the same machine and toolchain.

### Local

| Measure | Before | After |
|---|---|---|
| Clean `cargo test --workspace --all-features --no-run` | 140 s wall, 1,183 s unit time | **128 s** wall, 890 s unit time |
| ... integration-test binaries | 113 units, 419 s | **17 units, 85 s** |
| Incremental rebuild after touching `mb-engine/src/lib.rs` | 31 s | **13 s** |
| Incremental rebuild after touching the `mockingbird` binary crate | 2 s | 2 s |
| Warm `cargo test --workspace --all-features` | 221 s | **56 s** |
| Warm `cargo nextest run --workspace --all-features` | 29 s | **19 s** (17.5 s test time) |
| `target/` after the test build | 14 GB | **7.3 GB** |
| Test executables in `debug/deps` | 172, ≈ 9.7 GB | **37, ≈ 2.1 GB** |
| Tests (nextest, no doctests) | 1,627, 1 failing on macOS | **1,996, all passing** (44 skipped locally: Postgres, Redis, Kafka) |

Clean-build wall time is now dominated by two C build scripts, `openssl-sys`
(54 s, rdkafka `ssl-vendored`) and `aws-lc-sys` (41 s), which the program did not touch.

### CI (last six `ci.yml` runs on `main`)

| Job | Before | After |
|---|---|---|
| Whole run | 5 m 45 s – 9 m 41 s | **3 m 36 s – 6 m 28 s** (median 3 m 50 s) |
| `check` | 5 m 22 s – 6 m 01 s | **2 m 19 s – 3 m 15 s** |
| `console` | 2 m 45 s – 3 m 50 s | 1 m 57 s – 2 m 55 s |
| `e2e` | 2 m 13 s – 2 m 31 s | 3 m 29 s – 6 m 13 s |
| `protocol-features` | 1 m 07 s – 1 m 11 s | 1 m 10 s – 1 m 27 s (now also builds the docs and the slim binary) |
| `deny`, `semver`, `helm`, `msrv` | ≤ 40 s | ≤ 40 s |

`e2e` is now the critical path. Its time is the Docker image build (149 s of the
212 s run on `14e399e`, against 45–57 s at the baseline): almost every refinement
PR touched crate sources, so the image's cargo layer rarely hit its cache. CI-4 and
CI-5 in [plan.md](plan.md#out-of-scope-here) and an image build that caches
dependencies separately from workspace sources are the next levers.

### Lint and code inventory

| Measure | Before | After |
|---|---|---|
| `unwrap_used`/`expect_used` allow attributes | 143 | **0** (`expect_used` is denied too; tests are allowed through `clippy.toml`) |
| `#[expect]` in library code | 0 | 1: the module-level one in `mb-auth/src/testing.rs`, the test identity provider, listed for 0.2.0 |
| `#[allow(dead_code)]` in `src` | 15, plus 14 feature-gated | **0**, plus 2 feature-gated in `mb-protocols/src/lib.rs` (modules unused when no HTTP protocol feature is on) |
| `#[allow(clippy::too_many_arguments)]` | 6 | **0** |
| `#[allow(unreachable_patterns)]` | 1 | **0** |
| `sleep(` in `tests/` | 160 | 127 (the rest are bounded waits on external processes or are what the test measures) |
| `sleep(` in `src` | 42 | 47 (retry backoff and chaos latency added by Waves 3–7; none in a hot path) |

### Left open

- **0.2.0** (breaking, only on the user's go): [plan.md](plan.md#020-scope), plus
  what the waves added. That covers `#[non_exhaustive]` and private fields on the
  published structs; typed errors for the `String` validators (MB-STORE-STYLE-11),
  `RouteError` printing its cause twice and `StateError`; `new()` returning
  `Self` now that `new_shared` exists; moving the D4 commands into `ClientCommand`;
  base paths and `faker_seed` in the compiled bundle; and a
  `protocols.graphql.max_batch_operations` key. It also covers API-key revokes and
  cache resets on the change bus, `Arc` responses in the response cache and the
  live tail, and the `mb-auth` testing panics.
- **Product calls:** opt-in scenarios, chaos and limits in snapshots (STUB-3);
  rebuilding only the mocks of a changed LLM profile (PERF-8a); following
  `wsdl:import`; URL imports with relative `$ref`s.
- **Follow-ups:** the Shopify `Query.products` deep-recursion body (an ignored
  failing test, left to the LLM-validity work); `faker_seed` in the Admin API;
  storing an import job's tenant across restarts (a migration); event channels and
  webhooks using the per-call provider policy; mb-protocols PERF-4/5/6 and mb-import
  PERF-7 leftovers; `missing_docs` for mb-store's model fields.
- **Behaviour notes:** a retried webhook event can arrive after later ones; a
  deleted contract's version number can be reused; LLM answers with an invalid
  `uuid` or other asserted format are now labelled `repaired`; existing OIDC
  deployments now sign users out after 1 hour idle (`auth.session_idle_timeout: 0`
  restores the old behaviour); contracts uploaded before D6 keep empty non-JSON
  bodies until re-imported.
