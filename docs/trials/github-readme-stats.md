# Trial: github-readme-stats (Node, GitHub GraphQL), holdout runs (2026-11-11)

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

## Rerun on `main` at `0d591b1b` (2026-11-11, second holdout run)

The same app at `54a7985`, on `main` after the pool, GraphQL-reference, null-optional and lenient-import fixes. The mock was built by a fresh agent taking the README's path.

### How the mock was built

- **Install:** the README's Claude Code path, in a fresh clone of the app:
  - `claude plugin marketplace add rj-s-org/mockingbird --scope local`
  - `claude plugin install mockingbird@mockingbird --scope local`
  - Both commands took about a second. The plugin's MCP wrapper ran the `mockingbird` first on `PATH`, here a dev build of `0d591b1b`.
  - Local scope keeps the plugin out of other Claude Code sessions on the machine. A fresh `CLAUDE_CONFIG_DIR` would have isolated it too, but it is not logged in, and a headless agent can't log in.
- **Build agent:** headless Claude Code (`claude -p`, Sonnet 5.5) got one prompt and no hints:
  > I want to run this app (github-readme-stats) locally and render its cards without a GitHub token. Mock the GitHub API it calls (GraphQL and REST) with Mockingbird. Don't edit the app's source. Use OpenRouter with the model deepseek/deepseek-v4.1-flash-1017; my key is in OPENROUTER_API_KEY. When it works, tell me the exact command to run the app against the mock.
- **Ports:** another session's server held 9090, 50051 and 8888, so the agent's environment set `MB_PORT`, `MB_ADMIN_PORT`, `MB_GRPC_PORT` and `MB_ADMIN_URL` to free ports. The prompt didn't mention them. The proxy has no environment variable: the agent found 8888 taken and set `proxy.port: 28888` in the config itself.
- **Time:** **5.0 min** (302 s), 31 turns, $0.62 of Claude Code spend. It used skills, MCP and CLI only, with no edits to Mockingbird or the app.
- **What it built:**
  - Two hand-written contracts covering the fields the app reads, both with `hosts: [api.github.com]`: GraphQL with `route_prefix: /graphql`, REST with `/search`.
  - It made `languages` non-null in its SDL after top-langs crashed on a `null`.
  - It set `total_count` to at least 100 after a generated `0` made the app treat the count as a failed fetch.
  - It wrote no world description, and said so.
  - It moved its config from its session scratchpad to `~/.mockingbird/github-readme-stats/` and gave a `mockingbird server start … --proxy` command plus the app's proxy environment.

### Results

`flows.sh` drove 14 card requests through the unmodified app: stats, top-langs, five pins, a gist, repeats, an unknown user and an unknown repository.

| Run | Cards render | Pins | Calls | Sources | State/cache p50 | Model p50 / p95 | Tokens (prompt / completion) |
| --- | --- | --- | --- | --- | --- | --- | --- |
| `main` `0d591b1b` | 10/14 | 2/5 named pins; the unknown repository errors | 15 | 12 `llm`, 3 `cache`, 0 fallbacks | 2 ms | 1.52 s / 3.68 s | 19.4k / 1.9k |
| With #471 and #473 | 12/14 | 4/5 named pins (`denoland/deno`, G2b) | 15 | 13 `llm`, 2 `cache`, 0 fallbacks | 4 ms | 1.69 s / 3.23 s | 16.5k / 2.5k |

**Pins on `main`:**
- `octocat/hello-world` answered both `user` and `organization`, which the app reads as ambiguous.
- `rust-lang/rust` answered both null (6 completion tokens).
- `denoland/deno` answered `user.repository: null`.

**With the fixes:** the unknown user errors too. It gets an empty account whose nullable `repositories.nodes` is left out, so the app fails on `undefined` (G7).

The app's pin query sent straight to the data plane, 7 repositories, a new session for each sample:

| Build | Contract | Pins that render | Failures |
| --- | --- | --- | --- |
| `main` | agent's | 11/21 | 6 both roots null, 1 both found, 4 `user.repository` null |
| #471 + #473 | agent's (no interfaces) | **21/21** | none; p50 1.32 s, p95 2.86 s |
| #471 + #473 | agent's, with GitHub's `RepositoryOwner` interface on `User` and `Organization` | 19/21 | 2 `user.repository` null or private; p50 1.87 s, p95 3.55 s |

Unit tests: 241/242, the same `calculateRank` rounding failure as with no mock. They stub axios themselves.

### Failures and fixes

| # | Class | Failure | Fix |
| --- | --- | --- | --- |
| G1 | Engine, general | A joint answer for root fields that may all be null could be `{}`, and the model gave it for `user(login:)` + `organization(login:)`. The joint prompt's blanket "at most one of them exists" invited it. | #471: the blanket sentence is gone. Root fields that look up one thing form a lookup group, decided from the schema (different object types implementing one interface that declares every argument). A group answer finding none is repaired. |
| G2 | Engine, general | Root fields of one lookup group were both answered, by the model or by per-field generation after a failed joint answer. | #471: one per group is kept: the part answered from state or a scenario, else the first in the query. |
| G2b | Contract | The agent's hand-written SDL declares no interface on `User` and `Organization`, so no group forms. `denoland/deno` was answered by both roots once in the app flows (0 of 21 in the data-plane samples). | Not fixed. Candidate: the skills could ask agents trimming a schema to keep the interfaces its root fields' types implement. |
| G3 | Engine, general | A nested lookup by a value of its record (`repository(name:)`) came back `null`. | #473: the field's description tells the model the record exists. The schema still allows `null`, for scenarios, the world and stored state. |
| G4 | Skills gap | The agent wrote no world description, again, and nothing in the tool results pointed it to one. So stats, top-langs and pins for one user are generated separately (`no_identity`). | Not fixed. Candidate: `start_server`/`import_contract` results for a GraphQL mock without a world could say what a world description would make stateful. |
| G5 | Install and config | No environment variable or `start_server` field for the proxy port, while the data, admin and gRPC ports have `MB_*` variables. | Not fixed. The agent coped by editing the config. |
| G6 | Measure 7 | An unknown login gets an invented user, not GitHub's `NOT_FOUND` error. | None needed for the flows. A GraphQL contract declares no error, and without a world nothing says a login is unknown. |
| G7 | Model answer | A nullable list (`repositories.nodes`) is left out of an empty account, and the app reads `.filter` of `undefined`. | Not fixed; seen once in 14 requests. |

### Scorecard (measures 1–7)

| # | Measure | Target | `main` `0d591b1b` | With both fixes |
| --- | --- | --- | --- | --- |
| 1 | App's tests and main flows pass | 100% | ✗ 10/14 card requests (pins) | ✗ 12/14 (G2b, G7); pin query 21/21 on the agent's contract |
| 2 | Consistency the flows rely on | 100% | Partly: repeats are identical (`cache`); different queries for one user are generated separately (G4); no writes | Same |
| 3 | Agent build ≤ 15 min, skills/MCP/CLI only | ≤ 15 min | ✓ 5.0 min, 31 turns | n/a (same build) |
| 4 | State/pool latency | p50 < 50 ms | ✓ 2 ms (n = 3) | ✓ 4 ms (n = 2) |
| 5 | Model latency | p50 < 2 s, p95 < 5 s, rare | p50 1.52 s and p95 3.68 s ✓; not rare (12 of 15) | p50 1.69 s and p95 3.23 s ✓; not rare (13 of 15) |
| 6 | Model spend per session | < $0.05 | ✓ about $0.002 | ✓ about $0.002 |
| 7 | Errors: unknown ids, invalid input | declared status and body | n/a: no declared GraphQL errors (G6) | Same |
