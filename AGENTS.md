# AGENTS.md

Guidance for coding agents. Two cases:

## Using Mockingbird (mocking an API for the user)

Read [`plugins/mockingbird/skills/mockingbird/SKILL.md`](plugins/mockingbird/skills/mockingbird/SKILL.md) first; it routes to the setup, mock-api, deploy and testing skills next to it. Prefer the MCP server (`mockingbird mcp`) when it is connected; otherwise every operation is a `mockingbird` subcommand that prints JSON. Never write an API key into a file or command: profiles reference environment variables. [docs/agents.md](docs/agents.md) covers installing into each agent.

## Working on this repository

- Rust workspace (`crates/*`), toolchain pinned in `rust-toolchain.toml`; the console is `crates/mb-console/web` (Node 22).
- Before proposing a change, run what CI runs (see [CONTRIBUTING.md](CONTRIBUTING.md#local-gates)):
  `cargo fmt --all --check`, `cargo clippy --workspace --all-targets --all-features -- -D warnings`, `cargo test --workspace --all-features`, and for console changes `npm run typecheck && npm run lint && npm test` in `crates/mb-console/web`.
- No `unwrap()`/`expect()` outside tests; Admin API errors are `application/problem+json`; secrets are references only.
- After changing the Admin API, regenerate the console client: `npm run gen:api` in `crates/mb-console/web`.
- New config keys go in `docs/configuration.md` and `docs/examples/mockingbird.full.yaml` (a test checks both).
- The agent skills in `plugins/mockingbird/skills` are embedded in the binary (`crates/mockingbird/src/agents.rs`); `plugins/mockingbird/bin/install.sh` must stay a copy of `install.sh`; plugin manifest versions follow the workspace version.
