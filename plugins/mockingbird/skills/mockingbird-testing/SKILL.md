---
name: mockingbird-testing
description: Use Mockingbird mocks in automated tests and CI - deterministic offline mode, seeded fixtures, isolated sessions per test, forced errors and latency, scenarios, asserting on the request log, and GitHub Actions setup. Use when the user writes integration or end-to-end tests against an external API, or wants a mock dependency in a pipeline.
---

# Mocks in tests and CI

## Make runs repeatable

- **No LLM in CI**: `--offline` serves faker data checked against the schema (an answer it cannot make valid says so in `X-Mockingbird-Fallback-Reason`); no key, no cost, no network. It is seeded: the same contract and request give the same body on every run. Set a mock's `faker_seed`, or send `X-Mockingbird-Seed: <n>` on one request, for other data. Keep an LLM profile for local exploration.
- **Isolate each test** with its own session: send `X-Mockingbird-Session: <test-id>` (gRPC metadata `x-mockingbird-session`). State created in one session is invisible to others.
- **Known data**: export a good session once and seed it in each run.

```sh
mockingbird export-fixtures --mock orders --session demo > tests/fixtures/orders.json
mockingbird seed-fixtures --mock orders --session ci tests/fixtures/orders.json
```

  (MCP: `export_session`, `seed_session`.) Fixtures are `[{resource_type, id, data}]`.
- **Edge cases on demand**: `X-Mockingbird-Status: 503`, `X-Mockingbird-Latency: 2s`, or a named scenario (`X-Mockingbird-Scenario: out-of-stock`) declared in `mockingbird.yaml` so it is versioned with the tests.
- **Resilience tests**: per-mock `chaos` (`error_rate`, `latency_jitter`, `drop_stream_rate`); set `MB_CHAOS_SEED` for a reproducible sequence; `X-Mockingbird-Chaos: off` exempts a request.

## Assert on what the client sent

`GET /api/v1/workspaces/default/mocks/<mock>/requests?session=<id>` (MCP `list_requests`, filters `session`, `operation`, `source`, `kind`) returns the recorded calls with redacted bodies, matched operation ids and statuses. Assert on it after the code under test runs, e.g. that exactly one `createOrder` was made.

## GitHub Actions

```yaml
- name: Install Mockingbird
  run: |
    curl -fsSL https://raw.githubusercontent.com/rj-s-org/mockingbird/main/install.sh | sh
    echo "$HOME/.mockingbird/bin" >> "$GITHUB_PATH"
- name: Start mocks
  run: mockingbird server start --offline -c tests/mockingbird.yaml
- name: Seed
  run: mockingbird seed-fixtures --mock orders --session ci tests/fixtures/orders.json
- run: npm test     # point the client at http://127.0.0.1:8080/orders
- if: always()
  run: mockingbird server logs -n 200; mockingbird server stop
```

Or as a service container: `ghcr.io/rj-s-org/mockingbird` with `serve --host 0.0.0.0 --insecure-no-auth --offline -c /work/mockingbird.yaml` (CI networks only).

## Test frameworks

Prefer proxy mode, which needs no change to the code under test: give the mock `hosts: [api.example.com]` and run the suite under `mockingbird run -c mockingbird.yaml -- npm test` (it starts the server and proxy, exports the variables, stops the server and keeps the suite's exit code). Or start the server with the proxy in a global setup step (`mockingbird server start --proxy -c mockingbird.yaml`, MCP `start_server {proxy: true}`), and export the variables from `mockingbird proxy env` (MCP `proxy_env`) in the test process's environment (`eval "$(mockingbird proxy env)"`). For browser tests, give Playwright `proxy: {server: process.env.HTTPS_PROXY}` and `ignoreHTTPSErrors: true`, or start Chrome with `mockingbird proxy browser`. Where the client ignores proxy variables, point its base URL at the mock instead (`http://127.0.0.1:8080/<mock>`). Set the session header per test. For gRPC, dial `127.0.0.1:50051` with plaintext; for GraphQL, POST to `/<mock>/graphql`. `mockingbird validate <contract>` in CI catches contract errors before the mock is served.
