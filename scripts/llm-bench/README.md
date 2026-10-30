# LLM model benchmark harness

This harness runs OpenRouter models through Mockingbird's real LLM code paths: the engine workload through `mb-evals` and the `mockingbird serve` suites. It records latency, tokens and cost for every call and keeps spend under a hard budget. It uses only the Python 3.9+ standard library.

| file | role |
|---|---|
| `proxy.py` | Recording proxy in front of OpenRouter. It enforces the budget and measures latency and cost. |
| `run.py` | Orchestrator with three subcommands: `preflight`, `pilot` and `full`. |
| `server_suites.py` | The `mockingbird serve` workloads: consistency, SSE and smart import. |
| `grade.py` | Deterministic schema conformance grading of the samples. |
| `aggregate.py` | Joins all outputs into `results.json`, `results.csv` and `tables.md`. |

## The key

`OPENROUTER_API_KEY` must be in the environment (`source ~/.zshenv >/dev/null 2>&1`). Never print it or write it to a file. Only `proxy.py` and the `preflight` metadata calls read it. Child processes get dummy keys: `MB_EVAL_LLM_API_KEY=bench-dummy` for mb-evals and `OPENROUTER_API_KEY=bench-dummy` for the server suites. The proxy replaces the client's Authorization header with the real key.

## Re-running

```sh
source ~/.zshenv >/dev/null 2>&1
cargo build --release -p mb-evals -p mockingbird-cli

# 1. Model metadata and prices -> roster.json; key credit numbers -> key-before.json
python3 scripts/llm-bench/run.py preflight

# 2. Pilot: ~10 engine calls per model, then a printed cost projection
python3 scripts/llm-bench/run.py pilot --calls 10 --repeat 3
#    (--models a,b,c to limit it; results in <out>/pilot/pilot.json)

# 3. Full run: cheapest first, 3 models at a time, Inkling models last and alone
python3 scripts/llm-bench/run.py full --repeat 3 --parallel 3 --concurrency 4

# 4. Grade and aggregate (see grade.py / aggregate.py --help)
```

mb-evals runs with a default LLM profile's settings, as the server does: a 60 s timeout, `max_tokens` 16,384, temperature 0.7 and the default output budget (1,500 tokens). `--llm-timeout-secs`, `--max-tokens` and `--output-budget` override them. GraphQL targets are called with the selection set their manifest entry gives (`selections` in `docs/benchmarks/workloads.json`, see `crates/mb-evals/src/selection.rs`), as a real client's query would. `--prior-ledger <earlier calls.jsonl(.gz)>` (repeatable) counts an earlier run's spend toward `--budget-usd`. `--output-mode none|json_object|json_schema` sets the engine's `output_mode` (default `json_schema`, the server default). A model listed as `<id>@nojson` always runs with `none`, and the proxy also strips `response_format`.

The 2026-10-29 run predates these settings: it ran at 2,048 tokens and 30 s, with no GraphQL selections.

All results go to `--out`, which defaults to `docs/benchmarks/2026-10-29`. Relative paths resolve against the repository root, and children run with the root as their working directory.

| path under `--out` | contents |
|---|---|
| `roster.json` | The requested models with per-token `pricing`, `supported_parameters`, `response_format` and `structured_outputs`. Models OpenRouter does not list have `found: false`. |
| `key-before.json`, `key-after.json` | Only `usage`, `usage_daily` and `limit_remaining`. |
| `calls.jsonl` | The proxy ledger for the full run, one line per client call. The spec lists the fields. |
| `engine/<slug>/` | Per model: `samples.jsonl`, `report.json`, `report.md` and `mb-evals.log`. |
| `server/<slug>/` | Per model: `server.jsonl` and `server_suites.log`. |
| `samples.jsonl`, `server.jsonl` | Every model's samples and server checks merged into one file each. `run.py` adds `model` and `model_slug` to each sample line. |
| `run.json` | Run state: order, projections, each model's status (`done`/`partial`/`failed`/`skipped`), why models were skipped, and spend. |
| `pilot/` | The pilot's own ledger (`calls.jsonl`), slice manifest, `pilot.json` and per-model mb-evals output. |
| `proxy.log` | The proxy's log, which never contains the key. |
| `BUDGET_TRIPPED` | Exists once the budget has been hit. |

The model slug is the model id with `/` replaced by `__`. Proxy labels have the form `<slug>~<workload>`. The workload is `engine` for the full run, `pilot` for the pilot, and the suite name for the server suites.

## Budget

The proxy stops spending. Before each call it reserves an estimate of the call's cost: prompt characters / 3 × input price, plus `max_tokens` (default 4096) × output price. It refuses the call with 402 when the following would reach `--budget-usd` (default 9.0):

    ledger spend + prior-ledger spend + in-flight reservations + this call's estimate

A refusal records `error_kind: budget` and creates `BUDGET_TRIPPED`. After that the proxy refuses every call, including after a restart. To reset, delete the file and raise the budget on purpose.

The budget covers the pilot and the full run together. `run.py` passes the other phase's ledger to the proxy as `--prior-ledger`, and a `BUDGET_TRIPPED` next to a prior ledger counts as a trip too.

On SIGTERM or Ctrl-C the proxy stops accepting calls and waits (up to its 120 s upstream timeout) for in-flight calls to reach the ledger, so a restart counts their cost. `run.py` starts it in its own session and stops it after the children. A kill -9 still loses the in-flight calls' records.

`full` also checks the budget before it starts each model. Projections come from `pilot/pilot.json` when it exists and from roster prices otherwise. If spend so far, plus the unspent projections of the models still running, plus the next model's projection would cross the budget, `full` skips that model and every model after it.

## Stopping

Press Ctrl-C or create `<out>/ABORT`. No new model starts, and the running mb-evals and server-suite processes get SIGTERM. Remove `ABORT` before the next run.

Children never see the key: `run.py` drops `OPENROUTER_API_KEY`, any variable with the same value and every `MB_LLM_*` / `MB_EVAL_LLM_*` variable (mockingbird's zero-config profile defaults to openrouter.ai, which would bypass the proxy). `server_suites.py` drops every `MB_*` variable from the server's environment. The proxy redacts the key from anything it echoes (error excerpts and error bodies).

## Proxy details

- `GET /stats` returns spend, call count, the trip flag and per-label totals. `GET /healthz` returns 200.
- When the client did not ask for streaming, the proxy upgrades the call to streaming (turn this off with `--no-stream-upgrade`). This lets it measure TTFT, the time to the first non-empty `delta.content`. TTFR is the time to the first reasoning delta. Both are measured from when the attempt that succeeded had its upstream connection up (TCP and TLS are excluded: a real client reuses pooled connections). `total_ms` is measured from when the client's request arrived.
- OpenRouter's keep-alive comments (`: OPENROUTER PROCESSING`) are skipped. The proxy reads usage, including `usage.cost`, from the final chunk. A mid-stream `error` object, or `finish_reason: "error"`, becomes a 502 carrying the error body. If the client asked for a stream, the 200 header has already gone out, so the error passes through inside the stream.
- If the upstream rejects `reasoning: {"enabled": false}` (400/404/422 with "reasoning" in the body), the proxy retries once without it and remembers that for the model. It does the same for `response_format` (400/422 that mentions response_format or JSON mode).
- When there is no usage chunk (cut stream or timeout), cost is estimated from the prompt plus the output seen so far, with `cost_source: "estimate"`. A model the roster does not price is reserved at deliberately high fallback prices.
- `error_kind: empty` means both content and reasoning were empty. `refusal` means `finish_reason == "content_filter"` or a non-null `refusal`.

## Testing without spend

Both scripts accept a hidden `--upstream URL` flag that points the proxy at a fake chat endpoint. With it, `run.py` skips the key calls. `fake_llm.py` can stand in for OpenRouter:

```sh
python3 scripts/llm-bench/fake_llm.py --port 18999 &
env -u OPENROUTER_API_KEY python3 scripts/llm-bench/run.py pilot --out /tmp/bench-test \
  --models a/x,b/y --upstream http://127.0.0.1:18999/api/v1/chat/completions
```
