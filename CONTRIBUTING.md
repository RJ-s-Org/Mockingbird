# Contributing to Mockingbird

Thanks for helping. This guide covers the toolchain, the checks a change must pass, the
commit style, licensing and the two most common kinds of contribution: a new protocol
adapter and a new importer. Please read the [code of conduct](CODE_OF_CONDUCT.md) first,
and report security issues privately as described in [SECURITY.md](SECURITY.md).

## Toolchain

- **Rust 1.97** (pinned in `rust-toolchain.toml`, with `rustfmt` and `clippy`).
- **Node 22** for the console in `crates/mb-console/web`.
- Optional, for the gated integration tests: a local PostgreSQL server (`initdb`,
  `pg_ctl` on `PATH`) and `redis-server`. Kafka and AMQP tests run in CI against
  service containers and are skipped locally unless `MB_TEST_KAFKA_URL` /
  `MB_TEST_AMQP_URL` are set.
- Building with `--features kafka` (or `full`) compiles librdkafka and needs `cmake`, a
  C/C++ toolchain, `libssl-dev`, `zlib1g-dev` and `libcurl4-openssl-dev`.

## Local gates

These are the commands CI runs. Run them before opening a pull request; a pull request
is merged only when all of them pass.

```sh
# Rust
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
cargo check -p mockingbird --no-default-features   # the slim binary

# Postgres- and Redis-gated tests, against throwaway local servers
scripts/test-postgres.sh
scripts/test-redis.sh
# both at once
MB_TEST_RUNNER=scripts/test-postgres.sh scripts/test-redis.sh

# Console (from crates/mb-console/web)
npm ci
npm run typecheck
npm run lint
npm test
npm run build
npm run check:api     # the generated Admin API client matches the server
npm run e2e           # Playwright; run `npx playwright install chromium` once
```

`cargo test` without the scripts skips the Postgres and Redis tests (they read
`MB_TEST_POSTGRES_URL` and `MB_TEST_REDIS_URL`). After any Admin API change, run
`npm run gen:api` and commit the regenerated `src/api/schema.d.ts`; never edit it by hand.

Project rules the reviewers check:

- no `unwrap()` or `expect()` outside tests;
- Admin API errors are RFC 9457 `application/problem+json` documents;
- secrets are only ever references (`env:`, `file:`, `vault:`), never stored values;
- a new serde field on a stored type is `#[serde(default, skip_serializing_if = …)]`, so
  stored JSON and snapshots stay byte-identical;
- `mb-engine`, `mb-store` and `mb-state` stay free of axum; `mb-protocols` depends on
  neither `mb-store` nor `mb-auth`;
- user-visible behaviour is documented in `README.md` or `docs/`, and noted in
  `CHANGELOG.md` under `## [Unreleased]`.

## Commit style

Commits follow `type(scope): summary`, in the imperative, lower case, without a final
period:

```
feat(protocols): stream drops for gRPC server streams
fix(store): keep request-log order stable on Postgres
docs: describe the Redis bus
```

Types: `feat`, `fix`, `docs`, `test`, `refactor`, `perf`, `build`, `ci`, `chore`. The
scope is the crate without its `mb-` prefix (`engine`, `state`, `console`, …) or the
area (`events`, `channels`, `deploy`). Keep unrelated changes in separate commits.

## License

Mockingbird is dual-licensed under `MIT OR Apache-2.0` ([LICENSE-MIT](LICENSE-MIT),
[LICENSE-APACHE](LICENSE-APACHE)). Contributions are inbound=outbound: by submitting a
contribution you agree that it is licensed under the same `MIT OR Apache-2.0` terms,
without any additional terms or conditions. There is no CLA and no DCO sign-off.

## Adding a protocol adapter

Protocol adapters contain no generation logic: they decode traffic into an operation
call, run it through the shared pipeline and encode the generated response.

1. Make sure the contract format compiles to MCM operations for the protocol (see
   *Adding an importer* if it does not).
2. Add a module to `crates/mb-protocols/src/` behind a new cargo feature in
   `crates/mb-protocols/Cargo.toml`, add the feature to `default` if it is pure Rust,
   and to the `protocol-features` matrix in `.github/workflows/ci.yml`. Every feature,
   and none, must build on its own without warnings.
3. Route every call through the shared `call` pipeline so data-plane auth, tenancy,
   quotas, overrides, scenarios, the request log and metrics behave like every other
   protocol. Map override headers and forced statuses to the protocol's native form.
4. Mount it in `crates/mockingbird` where the other adapters are mounted, and add an
   end-to-end test to `crates/mockingbird/tests/protocols_e2e.rs`.
5. Document it in the README's protocol table and add a gallery example under
   `examples/gallery/`.

## Adding an importer

Deterministic compilers live in `crates/mb-import/src/`:

1. Add a `SourceFormat` variant and teach `detect_format`/`sniff` to recognise it from
   the bytes (never from the file name).
2. Compile the source into an `mb_mcm::Bundle` with a JSON Schema per operation;
   report problems as `ImportError::Invalid` with a JSON pointer.
3. Add a fixture under `fixtures/` and tests for detection, compilation and the
   resulting schemas (snapshots with `insta` where the output is large).

LLM-assisted importers (docs, HAR, Postman) produce drafts that a person reviews before
activation; they never activate a contract on their own.

## Changes to `mb-ext`

`mb-ext` is the extension API that downstream binaries build against, and it follows
semantic versioning from 1.0. Any change to it needs a semver review: say in the pull
request whether the change is additive or breaking, and expect the `cargo-semver-checks`
gate in CI to agree. Breaking changes need a major version and a maintainer's explicit
approval; prefer `#[non_exhaustive]` types and new methods with default implementations.

## Pull requests

Fill in the pull request template, link the issue it resolves, keep the change focused,
and include tests. For larger changes, open an issue (or a discussion) first so the
design can be agreed before you write the code.
