---
name: mockingbird-testing
description: Use Mockingbird mocks in automated tests and CI - deterministic offline mode, seeded fixtures, isolated sessions per test, forced errors and latency, scenarios, asserting on the request log, and GitHub Actions setup. Use when the user writes integration or end-to-end tests against an external API, or wants a mock dependency in a pipeline.
---

# Mocks in tests and CI

## Make runs repeatable

- **No LLM in CI**: `--offline` serves faker data that always matches the schema; no key, no cost, no network. Keep an LLM profile for local exploration.
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
    curl -fsSL https://raw.githubusercontent.com/rjs-git/mockingbird/main/install.sh | sh
    echo "$HOME/.mockingbird/bin" >> "$GITHUB_PATH"
- name: Start mocks
  run: mockingbird server start --offline -c tests/mockingbird.yaml
- name: Seed
  run: mockingbird seed-fixtures --mock orders --session ci tests/fixtures/orders.json
- run: npm test     # point the client at http://127.0.0.1:8080/orders
- if: always()
  run: mockingbird server logs -n 200; mockingbird server stop
```

Or as a service container: `ghcr.io/rjs-git/mockingbird` with `serve --host 0.0.0.0 --insecure-no-auth --offline -c /work/mockingbird.yaml` (CI networks only).

## Test frameworks

Point the client's base URL at the mock (`http://127.0.0.1:8080/<mock>`), start the server in a global setup step, and set the session header per test. For gRPC, dial `127.0.0.1:50051` with plaintext; for GraphQL, POST to `/<mock>/graphql`. `mockingbird validate <contract>` in CI catches contract errors before the mock is served.
