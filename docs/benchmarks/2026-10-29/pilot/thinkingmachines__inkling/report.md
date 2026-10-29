# Mockingbird LLM evals

Model `thinkingmachines/inkling` at `http://127.0.0.1:62314/r/thinkingmachines__inkling~pilot/api/v1`, started 2026-10-29T18:05:10.886106+00:00.

| Metric | Value |
| --- | --- |
| Calls | 3 |
| Schema-valid rate | 0.0% |
| First-pass rate | 0.0% |
| Repair rate | 0.0% |
| Fallback rate | 100.0% |
| p50 latency | 3842 ms |
| p95 latency | 4310 ms |
| Prompt tokens per call | 1363.3 |
| Completion tokens per call | 308.7 |

## Per contract

| Contract | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| graphql/ecommerce-shopify-admin | 1 | 0 | 0 | 1 | 3842 |
| grpc/route-guide | 1 | 0 | 0 | 1 | 1828 |
| rest/payments-stripe | 1 | 0 | 0 | 1 | 4310 |

## Per API type

| API type | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| graphql | 1 | 0 | 0 | 1 | 3842 |
| grpc | 1 | 0 | 0 | 1 | 1828 |
| rest | 1 | 0 | 0 | 1 | 4310 |

## Per domain

| Domain | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| ecommerce | 1 | 0 | 0 | 1 | 3842 |
| generic | 1 | 0 | 0 | 1 | 1828 |
| payments | 1 | 0 | 0 | 1 | 4310 |
