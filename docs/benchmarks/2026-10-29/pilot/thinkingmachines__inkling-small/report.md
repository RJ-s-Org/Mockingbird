# Mockingbird LLM evals

Model `thinkingmachines/inkling-small` at `http://127.0.0.1:62332/r/thinkingmachines__inkling-small~pilot/api/v1`, started 2026-10-29T18:06:09.585024+00:00.

| Metric | Value |
| --- | --- |
| Calls | 10 |
| Schema-valid rate | 80.0% |
| First-pass rate | 70.0% |
| Repair rate | 10.0% |
| Fallback rate | 20.0% |
| p50 latency | 1408 ms |
| p95 latency | 29590 ms |
| Prompt tokens per call | 1261.0 |
| Completion tokens per call | 375.3 |

## Per contract

| Contract | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| asyncapi/commerce-events-kafka | 1 | 0 | 1 | 0 | 6004 |
| asyncapi/orders-amqp-2.6 | 1 | 1 | 0 | 0 | 833 |
| graphql/devtools-github | 1 | 0 | 0 | 1 | 20654 |
| graphql/ecommerce-shopify-admin | 1 | 1 | 0 | 0 | 3279 |
| grpc/otlp-trace-service | 1 | 1 | 0 | 0 | 906 |
| grpc/route-guide | 1 | 1 | 0 | 0 | 632 |
| rest/payments-paypal-orders | 1 | 0 | 0 | 1 | 29590 |
| rest/payments-stripe | 1 | 1 | 0 | 0 | 4657 |
| soap/accounts-rpc | 1 | 1 | 0 | 0 | 1408 |
| soap/calculator | 1 | 1 | 0 | 0 | 467 |

## Per API type

| API type | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| asyncapi | 2 | 1 | 1 | 0 | 6004 |
| graphql | 2 | 1 | 0 | 1 | 20654 |
| grpc | 2 | 2 | 0 | 0 | 906 |
| rest | 2 | 1 | 0 | 1 | 29590 |
| soap | 2 | 2 | 0 | 0 | 1408 |

## Per domain

| Domain | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| devtools | 2 | 1 | 0 | 1 | 20654 |
| ecommerce | 3 | 2 | 1 | 0 | 6004 |
| generic | 3 | 3 | 0 | 0 | 1408 |
| payments | 2 | 1 | 0 | 1 | 29590 |
