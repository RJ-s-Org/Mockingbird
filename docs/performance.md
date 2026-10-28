# Performance

Mockingbird's throughput is bounded by the LLM provider: a generated response takes as long as the provider takes (seconds, typically), and `limits.max_concurrent_generations` caps how many run at once. What Mockingbird itself adds is measured with the **offline bench**: the same server stack, with the schema faker answering instead of an LLM, so the numbers show the cost of decoding, admission, state, validation, encoding and the request log.

## Method

`mb-evals bench` (in [`crates/mb-evals`](../crates/mb-evals)) builds the real server stack in process for one contract with `--offline` semantics, binds it on loopback, and drives it with `reqwest`:

- the contract is `fixtures/openapi/orders-3.1.yaml` by default;
- requests round-robin over the contract's non-streaming `GET` and `POST` operations, with every path parameter set to `1` and faker-built request bodies;
- `--concurrency` workers (default 32) send back to back for `--duration` (default 10 s);
- the request log recorder is on, as in production;
- latency is measured per request on the client; p50, p95 and p99 are nearest-rank percentiles; errors are transport failures and 5xx answers.

The result is printed and written as JSON (`BenchReport`).

## Results

| Field | Value |
| --- | --- |
| Machine | 4 vCPU (Intel Xeon, 2.8 GHz) Linux sandbox, client and server on the same machine |
| Build | `--release` |
| Contract | `fixtures/openapi/orders-3.1.yaml` |
| Concurrency | 32 |
| Duration | 10.02 s |
| Requests | 8 952 (0 errors) |
| Throughput | 893.2 req/s |
| p50 | 33.40 ms |
| p95 | 53.33 ms |
| p99 | 58.27 ms |

These are a baseline for spotting regressions, not a capacity promise: the client shares the CPUs with the server, and the in-memory SQLite store and request log run in the same process. On dedicated hardware with Postgres, scale out data nodes for more throughput (they share nothing but the stores).

### Under load in a cluster

The bench runs one node with an in-process bus. In a cluster, per-request bus traffic (state changes behind subscriptions and change feeds, request-log notices for the console's live view) shares each node's broadcast capacity (1024 events) with the control events. A node that falls behind runs a full local `Resync`, and every `Resync` rebuilds all its mocks: rate-limit buckets reset to full and event channels restart, disconnecting WebSocket channel subscribers. At high request rates per node, watch `mb_bus_events_total{type="resync"}` and scale out before lag sets in; see [frequent resyncs](operations/runbook.md#frequent-resyncs).

## Rerunning

```sh
cargo run --release -p mb-evals -- bench --duration 10s --out target/bench.json
```

Options:

| Flag | Default | Meaning |
| --- | --- | --- |
| `--contract` | `fixtures/openapi/orders-3.1.yaml` | Contract to serve. |
| `--duration` | `10s` | How long to send. |
| `--concurrency` | `32` | Concurrent client workers. |
| `--out` | `bench.json` | Where the JSON report goes. |

Compare runs on the same machine only, with nothing else busy. When you change the request path, run the bench before and after and quote both numbers in the pull request.

## LLM quality

`mb-evals run` measures the generation pipeline against a real provider (schema-valid, repair and fallback rates, and p95 latency per contract). It runs weekly in CI (Mondays) (`.github/workflows/evals.yml`) and fails when a rate crosses its threshold; see `mb-evals run --help`.

## Latency against a real LLM

[`scripts/trial-bench.py`](../scripts/trial-bench.py) measures what an app waits for: it serves the field-trial contracts (GitHub REST and GraphQL, TMDB, Stripe) with `mockingbird serve` and a real provider, sends each case in fresh sessions (no cache), then once more as a repeat, and prints p50, max, sources and per-case checks. It needs the `mockingbird-trials` checkout and `OPENROUTER_API_KEY`:

```sh
cargo build --release -p mockingbird-cli
scripts/trial-bench.py --trials ../mockingbird-trials --runs 5
```

OpenRouter `deepseek/deepseek-v4.1-flash`, 2026-10-28, 3 runs before and 5 after the output budget, cached fallbacks, per-field GraphQL state and reasoning off ([#57](https://github.com/RJ-s-Org/Mockingbird/pull/57)):

| Case | p50 before | p50 after | max after | Sources after | Repeat after |
| --- | ---: | ---: | ---: | --- | --- |
| GitHub `search/commits` | 29.8 s | 9.4 s | 29.0 s | llm 5 | cache, 0.01 s |
| GitHub GraphQL stats | 1.7 s | 0.8 s | 1.4 s | llm 5 | cache |
| GitHub GraphQL pin (2 root fields) | 2.4 s | 1.2 s | 3.0 s | llm 5 | cache (was a new LLM call) |
| TMDB movie detail, all `append_to_response` parts | 51.0 s | 6.2 s | 14.9 s | llm 2, patched 3 | cache |
| TMDB popular | 15.3 s | 7.8 s | 19.6 s | llm 5 | cache |
| Stripe create PaymentIntent | 3.5 s | 1.7 s | 2.9 s | llm 4, patched 1 | n/a (a create) |

Provider latency varies run to run by 2–3×, so compare p50s from runs on the same day.
