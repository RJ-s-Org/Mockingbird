# Mockingbird LLM evals

Model `minimax/minimax-m3.1-flash` at `http://127.0.0.1:58282/r/minimax__minimax-m3.1-flash~pilot/api/v1`, started 2026-10-30T07:28:59.262923+00:00.

| Metric | Value |
| --- | --- |
| Calls | 10 |
| Schema-valid rate | 100.0% |
| First-pass rate | 80.0% |
| Repair rate | 20.0% |
| Fallback rate | 0.0% |
| p50 latency | 591 ms |
| p95 latency | 31256 ms |
| Prompt tokens per call | 842.8 |
| Completion tokens per call | 796.1 |
| Streaming operations not evaluated | 3 |

## Per contract

| Contract | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| asyncapi/commerce-events-kafka | 1 | 0 | 1 | 0 | 26986 |
| asyncapi/orders-amqp-2.6 | 1 | 1 | 0 | 0 | 482 |
| graphql/devtools-github | 1 | 1 | 0 | 0 | 4934 |
| graphql/ecommerce-shopify-admin | 1 | 1 | 0 | 0 | 802 |
| grpc/otlp-trace-service | 1 | 1 | 0 | 0 | 542 |
| grpc/route-guide | 1 | 1 | 0 | 0 | 476 |
| rest/payments-paypal-orders | 1 | 1 | 0 | 0 | 31256 |
| rest/payments-stripe | 1 | 0 | 1 | 0 | 1817 |
| soap/accounts-rpc | 1 | 1 | 0 | 0 | 591 |
| soap/calculator | 1 | 1 | 0 | 0 | 459 |

## Per API type

| API type | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| asyncapi | 2 | 1 | 1 | 0 | 26986 |
| graphql | 2 | 2 | 0 | 0 | 4934 |
| grpc | 2 | 2 | 0 | 0 | 542 |
| rest | 2 | 1 | 1 | 0 | 31256 |
| soap | 2 | 2 | 0 | 0 | 591 |

## Per domain

| Domain | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| devtools | 2 | 2 | 0 | 0 | 4934 |
| ecommerce | 3 | 2 | 1 | 0 | 26986 |
| generic | 3 | 3 | 0 | 0 | 591 |
| payments | 2 | 1 | 1 | 0 | 31256 |
