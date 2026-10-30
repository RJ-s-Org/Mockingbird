# Mockingbird LLM evals

Model `qwen/qwen3.8-flash` at `http://127.0.0.1:58282/r/qwen__qwen3.8-flash~pilot/api/v1`, started 2026-10-30T07:28:27.108026+00:00.

| Metric | Value |
| --- | --- |
| Calls | 10 |
| Schema-valid rate | 100.0% |
| First-pass rate | 80.0% |
| Repair rate | 20.0% |
| Fallback rate | 0.0% |
| p50 latency | 3917 ms |
| p95 latency | 8680 ms |
| Prompt tokens per call | 700.3 |
| Completion tokens per call | 222.4 |
| Streaming operations not evaluated | 3 |

## Per contract

| Contract | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| asyncapi/commerce-events-kafka | 1 | 0 | 1 | 0 | 5606 |
| asyncapi/orders-amqp-2.6 | 1 | 1 | 0 | 0 | 1845 |
| graphql/devtools-github | 1 | 1 | 0 | 0 | 7381 |
| graphql/ecommerce-shopify-admin | 1 | 1 | 0 | 0 | 2482 |
| grpc/otlp-trace-service | 1 | 1 | 0 | 0 | 5854 |
| grpc/route-guide | 1 | 1 | 0 | 0 | 1442 |
| rest/payments-paypal-orders | 1 | 1 | 0 | 0 | 8680 |
| rest/payments-stripe | 1 | 0 | 1 | 0 | 3917 |
| soap/accounts-rpc | 1 | 1 | 0 | 0 | 2071 |
| soap/calculator | 1 | 1 | 0 | 0 | 6674 |

## Per API type

| API type | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| asyncapi | 2 | 1 | 1 | 0 | 5606 |
| graphql | 2 | 2 | 0 | 0 | 7381 |
| grpc | 2 | 2 | 0 | 0 | 5854 |
| rest | 2 | 1 | 1 | 0 | 8680 |
| soap | 2 | 2 | 0 | 0 | 6674 |

## Per domain

| Domain | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| devtools | 2 | 2 | 0 | 0 | 7381 |
| ecommerce | 3 | 2 | 1 | 0 | 5606 |
| generic | 3 | 3 | 0 | 0 | 6674 |
| payments | 2 | 1 | 1 | 0 | 8680 |
