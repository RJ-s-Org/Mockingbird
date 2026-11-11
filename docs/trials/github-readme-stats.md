# Trial: github-readme-stats (Node, GitHub GraphQL), holdout run (2026-11-11)

[anuraghazra/github-readme-stats](https://github.com/anuraghazra/github-readme-stats) at `54a7985` is an Express app that renders SVG cards (stats, top languages, repository pins, gists) from GitHub's GraphQL API, plus REST `search/commits` for `include_all_commits`. This is a **holdout** run on `main` at `d6e7a49f`, after record pools (#416). The mock was built by the user's agent from one prompt.

| | |
| --- | --- |
| APIs mocked | GitHub GraphQL (`api.github.com/graphql`) and REST `search/commits` |
| Contracts | Two small contracts the agent wrote, covering the fields the app queries |
| Model | OpenRouter `deepseek/deepseek-v4.1-flash-1017` |
| App edits | None. `PAT_1` is a dummy value: the app refuses to call out with no token at all. |

## How the mock was built (as the user's agent)

Headless Claude Code (Sonnet 5.5), in a fresh clone with `mockingbird agent install claude-code --scope project`, got one prompt: mock the GitHub API (GraphQL and REST) with Mockingbird, with no token and no source edits, using OpenRouter `deepseek/deepseek-v4.1-flash-1017`, and say how to run the app against it.

It finished in 116 s and 35 turns, using only the skills, MCP and CLI. It:

1. wrote `github-graphql.graphql` and `github-rest.yaml` by hand, covering only the fields the app reads;
2. served both mocks with `hosts: [api.github.com]` (GraphQL at `/`, REST at `/search`), plus a persona asking for every selected field to be filled (without it, `languages` and gist `owner` came back null and the cards errored);
3. ran the app unmodified through `mockingbird proxy env` and checked each card endpoint.

It wrote no world description, so the request log shows `exec_fallback: no_identity` on every GraphQL call: nothing is answered from state.

## Results

Twelve card requests ran through the unmodified app (`flows.sh`: stats, top-langs, five pins, a gist, then repeats and `include_all_commits`).

| Card | Result |
| --- | --- |
| Stats (`octocat`, `gaearon` with `include_all_commits`) | ✓ Renders (Rank S, Rank A). A repeat answers the same numbers (`cache`). |
| Top languages (`octocat`, `gaearon` compact) | ✓ Renders |
| Gist | ✓ Renders |
| Pins: `octocat/hello-world` | ✓ Renders |
| Pins: `vercel/next.js`, `torvalds/linux`, `rust-lang/rust`, `denoland/deno` | ✗ "Something went wrong" (F1) |

| Calls | Sources | Model-served latency | Tokens (prompt / completion) |
| --- | --- | --- | --- |
| 13 (12 GraphQL, 1 REST) | 9 `llm`, 4 `cache`, 0 fallbacks | p50 0.89 s, p95 3.8 s (n = 9); slowest is top-langs for `gaearon` | 10.7k / 1.5k |

The app's unit tests (241/242) stub axios, so they never reach the mock. The one failure (`calculateRank`, a rounding mismatch) fails the same way with no mock. The e2e suite (`jest.e2e.config.js`, pointed at the local app) passes 1/5. It compares cards with fixed data from real accounts, so generated data can't pass it. The one pass, WakaTime, went to the real WakaTime host.

## Failures

| # | Class | Failure | Repro: request → expected → got |
| --- | --- | --- | --- |
| F1 | Model answer, or inexpressible without a world | A pin's `user`/`organization` roots or the `repository` come back null, which the app reads as "not found" | `query { user(login: "vercel") { repository(name: "next.js") {…} } organization(login: "vercel") { repository(name: "next.js") {…} } }` → one root with the repository → both roots `null` (6 completion tokens). `torvalds/linux` and `denoland/deno` get `user.repository: null`. The same failure as the 2026-10-28 run's torvalds/linux pin. |
| F2 | Skills/MCP gap | The agent left GraphQL reads undescribed | Every call is `no_identity`, so the stats and top-langs queries for one user are generated separately, and a pin's owner is never a stored record. The skills didn't lead the agent to `world show` for a hand-written GraphQL contract. |
| F3 | Skills/MCP gap (minor) | The server was left on the default ports (8080, 9090, 8888) | Another server on the same machine would collide; the agent didn't pick ports. |

## Scorecard (measures 1–7)

| # | Measure | Target | Result |
| --- | --- | --- | --- |
| 1 | App's tests and main flows pass | 100% | ✗. 8 of 12 card requests render. 4 of 5 new pin cards fail (F1). |
| 2 | Consistency the flows rely on | 100% | Partly. Repeats with the same arguments are identical (`cache`). Queries for the same user that differ are generated separately (F2). The app writes nothing. |
| 3 | Agent build ≤ 15 min, skills/MCP/CLI only | ≤ 15 min | ✓ 116 s, 35 turns, no app or Mockingbird edits |
| 4 | State/pool latency | p50 < 50 ms | ✓ p50 4 ms (n = 4, all `cache`) |
| 5 | Model latency | p50 < 2 s, p95 < 5 s, rare | p50 0.89 s and p95 3.8 s ✓. Not rare: 9 of 13 calls are model-served (no world, so no state) |
| 6 | Model spend per session | < $0.05 | ✓ about $0.001 (10.7k prompt / 1.5k completion at the listed `-1017` price). With the agent's own checks: 25 calls, about $0.002. |
| 7 | Errors: unknown ids, invalid input | declared status and body | Not exercised: the app's flows send neither. |

**Do the fixes generalize?** The build itself does: one prompt, two minutes, the proxy and host routing with no source edits, and no fallbacks. Record pools don't help this app, because its reads are GraphQL with no world and its one REST list is scoped. What's left is the null-root answer (F1) and the missing world (F2).
