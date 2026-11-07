# AGENTS.md

Guidance for coding agents. Two cases:

## Using Mockingbird (mocking an API for the user)

Read [`plugins/mockingbird/skills/mockingbird/SKILL.md`](plugins/mockingbird/skills/mockingbird/SKILL.md) first; it routes to the setup, mock-api, deploy and testing skills next to it. Prefer the MCP server (`mockingbird mcp`) when it is connected; otherwise every operation is a `mockingbird` subcommand that prints JSON. Never write an API key into a file or command: profiles reference environment variables. [docs/agents.md](docs/agents.md) covers installing into each agent.

## Working on this repository

- Rust workspace (`crates/*`), toolchain pinned in `rust-toolchain.toml`; the console is `crates/mb-console/web` (Node 22).
- Before opening a pull request, run `scripts/gate.sh`: fmt, plus clippy, nextest and doctests for the crates you changed and their direct dependents, the console checks if you touched it, and the bench tests if you touched `scripts/llm-bench`. Don't run the full workspace locally. A pull request's CI runs the same scope plus the jobs its files affect, and main runs the full list after merge ([CONTRIBUTING.md](CONTRIBUTING.md#local-gates)).
- No `unwrap()`/`expect()` outside tests; Admin API errors are `application/problem+json`; secrets are references only.
- After changing the Admin API, regenerate the console client: `npm run gen:api` in `crates/mb-console/web`.
- New config keys go in `docs/configuration.md` and `docs/examples/mockingbird.full.yaml` (a test checks both).
- The agent skills in `plugins/mockingbird/skills` are embedded in the binary (`crates/mockingbird/src/agents.rs`); `plugins/mockingbird/bin/install.sh` must stay a copy of `install.sh`; plugin manifest versions follow the workspace version.

## Shipping a change (agents)

A change should go from opened pull request to merged in under 30 minutes, with **one CI run**. CI is right-sized: a pull request runs only the jobs its files can affect, and main runs everything after merge. Keep both review and CI bounded:

1. **One coherent change per pull request.** Small fixes in the same area that one reviewer can read together go in one PR. Unrelated changes don't.
2. **Gate, then open as a draft.** Run `scripts/gate.sh`, self-review the diff against CONTRIBUTING.md, then `gh pr create --draft`. A draft runs no CI.
3. **Review round 1 is exhaustive.** An independent reviewer (given the diff, not the author's reasoning) returns every finding in one pass, each marked *blocking* (wrong behaviour, broken rule, missing test for the change) or *non-blocking* (style, naming, follow-up ideas).
4. **Review round 2 only verifies** the blocking fixes. It raises nothing new unless a fix introduced a bug. Non-blocking findings go on a follow-up list (an issue or the next PR), never into another round.
5. **No round 3.** If blockers remain after round 2, stop and escalate to whoever is coordinating (the orchestrator session or the user) instead of looping.
6. **On approval, run CI once:** rebase onto `origin/main` only if the PR conflicts, then `gh pr ready` and `gh pr merge --squash --auto`. Fix pushes made during review cost no CI because the PR is still a draft.
7. **Don't rebase or merge `main` in otherwise.** `main` doesn't require up-to-date branches, and every push to a ready PR is a CI run.
8. **Build independent changes in parallel** in their own worktrees. Merge PRs that touch the same files one at a time.
