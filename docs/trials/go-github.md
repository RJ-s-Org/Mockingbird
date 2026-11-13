# Trial: go-github digest (Go, GitHub REST), fresh agent (2026-11-12)

[`examples/apps/go-github`](../../examples/apps/go-github) is a small Go package. It lists a repository's open issues and pull requests with [go-github](https://github.com/google/go-github) v66 and files a "Triage digest" issue.

Its test starts `mockingbird serve --offline` from the project's `mockingbird.yaml` on fixed ports (data 49080, admin 49090, gRPC 49051, proxy 49888). It builds its HTTP client from `mockingbird proxy env --json` and checks two things:
- **the digest**: an issue is posted;
- **the rate limit**: with the `rate-limited` scenario, the client gets a 403 that mentions the rate limit.

For this run the app was copied into a fresh project without its `mockingbird.yaml`, so the agent had to write one. The binary was `main` at `b927dcc9`, with the plugin from the same commit.

| | |
| --- | --- |
| APIs mocked | GitHub REST on `api.github.com`: list issues, list pull requests, create issue |
| Contract | The catalog's `github-rest` example, a hand-written subset of GitHub REST |
| Model | None: the app's test serves the mock with `--offline` (faker) |
| App edits | None to the app. This PR changes the example's rate-limit check (below). |

## How the mock was built (as the user's agent)

A fresh Claude Code (`claude -p`, Sonnet 5.5) had only the Mockingbird plugin and got one prompt: **"mock the GitHub API for this app"**. That prompt does not name Mockingbird, and an agent given it later wrote an `httptest` fake instead. Reruns use this prompt, verbatim:

> This Go package posts a triage digest to GitHub with go-github. Mock the GitHub API with Mockingbird so I can run it locally without a GitHub account or token, and write a Go test that runs Run against the mock, including how the app handles GitHub's rate limit. When done, tell me how to run it.

It asks for more than the first prompt (a Go test and the rate-limit path), so rerun times and costs are not comparable with the figures below.

It finished in **78 s and 13 turns** ($0.36). Its steps:
1. It read the README, `digest.go` and the test, then ran `doctor` and `list_examples`.
2. It ran `init_example github-rest` into a scratch folder, then copied in the `mockingbird.yaml` and the contract, without the example's README. It dropped the example's webhook block, which posts to a local receiver this app doesn't have.
3. It ran `go test`. The digest passed. The rate-limit subtest failed, and the agent explained why correctly (F1) instead of changing the test or the mock.

## Results

`mockingbird verify -c mockingbird.yaml --run "go test -count=1 ./..."`:

| Check | Before | With this PR |
| --- | --- | --- |
| Posts a digest issue | ✓ | ✓ |
| Surfaces GitHub's rate limit | ✗ `want a 403 ErrorResponse, got … 403 API rate limit exceeded … [rate limit was reset -16s ago]` | ✓ |

`verify --run` saw no requests in its window, because the test starts its own server on its own ports instead of using the one `verify` runs. Only the test's exit code counts for this app.

## Failures

| # | Class | Failure | Repro: request → expected → got |
| --- | --- | --- | --- |
| F1 | App's test (fixed here) | The catalog's `rate-limited` scenario answers as GitHub does: 403 with `X-RateLimit-Remaining: 0` and the other limit headers. go-github turns that into a `*github.RateLimitError`, but the example's test accepted only `*github.ErrorResponse`, which only a 403 without those headers produces. | `GET /repos/fernwood-labs/trellis/issues` with `X-Mockingbird-Scenario: rate-limited` → the test's 403 rate-limit check passes → `RateLimitError` fails `errors.As(err, &*ErrorResponse)`. The test now accepts either. It passes with the example's own scenario (no headers) and with the catalog's. |
| F2 | Catalog example (minor) | The scenario sends no `X-RateLimit-Reset`, so go-github's `Rate.Reset` is the zero time; the "-16s" is how go-github formats the time until a zero reset, not a clock offset | Open. A scenario header is a fixed string, so a reset time in the future needs a computed header value. |

## Scorecard (measures 1–7)

| # | Measure | Target | Result |
| --- | --- | --- | --- |
| 1 | App's tests and main flows pass | 100% | ✓ both subtests of `TestDigest` with this PR (1 of 2 before, F1) |
| 2 | Consistency the flows rely on | 100% | n/a: the app writes one issue and reads nothing back |
| 3 | Agent build ≤ 15 min, skills/MCP/CLI only | ≤ 15 min | ✓ 78 s, 13 turns, from a catalog example |
| 4 | State/pool latency | p50 < 50 ms | Not measured: the lists are faker answers, offline, with nothing served from state; the whole test takes about 1.1 s |
| 5 | Model latency | p50 < 2 s, p95 < 5 s | n/a: offline |
| 6 | Model spend per session | < $0.05 | ✓ $0 |
| 7 | Errors: declared refusals | declared status and body | ✓ The `rate-limited` scenario's 403, message and `X-RateLimit-Remaining: 0` reach the client (the test checks all three) |
