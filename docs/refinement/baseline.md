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

Filled in when the program finishes.
