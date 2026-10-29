# Build and CI audit (verified)

Verified read-only against .github/workflows/*.yml, Cargo manifests, run 40907076695's log, the cache-usage API and ruleset 26063132 (required checks: check, console, deny, e2e, helm, msrv, protocol-features, semver). No cargo or npm was run.

### CI-1: Drop the duplicate custom-ext test step (ci.yml:89-90), high, S
`examples/custom-ext` is a workspace member (Cargo.toml:3), so `cargo test --workspace --all-features` already runs its unit tests, tests/ext.rs and doctests (log 16:02:59 and 16:04:05). The `-p` step resolves mockingbird-cli with default features (mqtt+amqp, no kafka). The log shows different artifact hashes (`-ec…` and then `-73…`), so the step rebuilds the workspace crates and then runs the same tests again. It took 16:04:05 to 16:04:31, about 26s on the critical path.
Fix: delete the step. A default-features build of the example is not covered anywhere else, because the slim check is `--no-default-features`. If that coverage matters, add `cargo check -p mockingbird-custom-ext-example` off the critical path (see CI-3).
Risk: very low.

### CI-2: Run tests with cargo-nextest (ci.yml:88), high, M
The log shows the test build finishing at 16:02:01 and the doctests starting at 16:04:03. That is about 122s of test binaries run one at a time on the 8-vCPU runner. Fix: install cargo-nextest with taiki-e/install-action pinned by SHA. Then run `cargo nextest run --workspace --all-features --profile ci` and `cargo test --doc --workspace --all-features`. Commit `.config/nextest.toml` with a `ci` profile, plus a serial test-group for the cross-node binaries (mb-state bus/leader, ha_cluster). That file also serves local runs (folds in CI-16). Expected saving is about 60-90s; the floor is the slowest binary, bus.rs at about 26s.
Risk: concurrency flakes on shared Postgres, Redis or Kafka. Tests already use per-test schemas and topics, and the test-group contains the rest. Running each test in its own process also isolates the `set_var` tests (mb-ext defaults.rs and default_extensions_vault_env.rs, webhooks_e2e.rs).

### CI-3: Move `cargo doc` and the slim check to protocol-features (ci.yml:91-94), medium, S
Both steps re-check every workspace crate after clippy, which adds 31-36s to check. protocol-features (2 vCPU, about 70s, required) already installs libcurl. mb-console's build.rs writes a placeholder dist, so docs do not need Node. Keep the job name. Expect protocol-features to grow to about 2.5-3 min on 2 vCPU, which is still shorter than check. Its `protocols` cache will grow to cover all-features dependencies, and the first run will be cold. Do not split these into a new job: it would re-pay setup and would not be a required check.

### CI-4: Start service containers in the background (ci.yml:20-54), low, M
'Initialize containers' takes 16-21s before checkout, and the Kafka log shows the broker up at 15:59:44, minutes before the tests. Start the containers with `docker run -d` or `docker compose up -d` early in the job. Then add a readiness wait (pg_isready, redis-cli ping, rabbitmq-diagnostics, and a Kafka port probe) just before the tests. Saves about 15-20s.
Risk: the readiness wait and the Kafka KRaft start-up must be explicit.

### CI-5: Console job on the large runner once check shrinks (ci.yml:123), low, S
console runs on `vars.CI_RUNNER` (2 vCPU) and takes 2.8-3.8 min. This is CPU-bound: the `check:api` cargo build plus vitest. After CI-1 to CI-3 it becomes the critical path. Move it to `vars.CI_RUNNER_LARGE` and set `CARGO_PROFILE_DEV_DEBUG: "0"`. Keep the job name. The cost is more expensive runner minutes.

### CI-7: Do not cancel main pushes (ci.yml:12), low, S
`cancel-in-progress: true` also applies to push on main, because the group falls back to github.ref. A cancelled main run never reports a CI result and never saves its cache. Use `cancel-in-progress: ${{ github.event_name == 'pull_request' }}`, and apply the same in examples, integrations, iac and npm.

### CI-8: GitHub Actions cache is over its 10 GB limit, mostly tag-scoped (advice for release.yml), medium, S
Cache usage is 10,304,780,550 bytes across 164 entries. By ref: refs/tags/v0.1.0 holds about 6.5 GB, claude/release-0.1.0 about 1.8 GB and main about 2.0 GB. Caches scoped to a tag cannot be restored by later runs, and they evict main's e2e buildkit cache (`scope=full`). Next release: rust-cache `save-if: main`, and buildkit `cache-to` only from main. For now, delete the tag and release-branch caches with `gh cache delete`. That step needs the user's go.

### CI-11: Consolidate integration-test binaries per crate, low, L
About 130 test binaries are linked separately; 31 of them link the full mockingbird-cli graph. The disk-full SIGBUS noted at ci.yml:60-61 comes from this. Merge the tests into `tests/it/main.rs` modules, at least for mockingbird-cli, mb-protocols, mb-import and mb-admin. Gate `required-features` tests with `#[cfg(feature)]`. Do this after CI-2. The saving has not been measured; the estimate is 20-30s of link time and several GB of target/.
Risk: test reorganisation churn, and shared global state within one binary.

### CI-14: integrations.yml path filter misses the crates the proxy depends on (integrations.yml:9-17), low, S
The filter is limited to `crates/mockingbird/src/**`. The action exercises proxy, report and verify, which depend on mb-admin, mb-engine, mb-protocols and Cargo.lock. Add those paths, as iac.yml does. This is a coverage fix that costs about 1 min of non-required runner time per Rust PR. (The CARGO_INCREMENTAL part of the original finding is dropped: rust-cache already sets CARGO_INCREMENTAL=0.)
