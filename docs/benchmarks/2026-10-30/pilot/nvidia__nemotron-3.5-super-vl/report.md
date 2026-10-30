# Mockingbird LLM evals

Model `nvidia/nemotron-3.5-super-vl` at `http://127.0.0.1:58282/r/nvidia__nemotron-3.5-super-vl~pilot/api/v1`, started 2026-10-30T07:28:00.111827+00:00.

| Metric | Value |
| --- | --- |
| Calls | 10 |
| Schema-valid rate | 100.0% |
| First-pass rate | 70.0% |
| Repair rate | 30.0% |
| Fallback rate | 0.0% |
| p50 latency | 675 ms |
| p95 latency | 11243 ms |
| Prompt tokens per call | 755.6 |
| Completion tokens per call | 185.2 |
| Streaming operations not evaluated | 3 |

## Per contract

| Contract | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| asyncapi/commerce-events-kafka | 1 | 0 | 1 | 0 | 1917 |
| asyncapi/orders-amqp-2.6 | 1 | 1 | 0 | 0 | 367 |
| graphql/devtools-github | 1 | 1 | 0 | 0 | 11243 |
| graphql/ecommerce-shopify-admin | 1 | 1 | 0 | 0 | 2125 |
| grpc/otlp-trace-service | 1 | 1 | 0 | 0 | 531 |
| grpc/route-guide | 1 | 1 | 0 | 0 | 509 |
| rest/payments-paypal-orders | 1 | 0 | 1 | 0 | 2306 |
| rest/payments-stripe | 1 | 0 | 1 | 0 | 1281 |
| soap/accounts-rpc | 1 | 1 | 0 | 0 | 675 |
| soap/calculator | 1 | 1 | 0 | 0 | 386 |

## Per API type

| API type | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| asyncapi | 2 | 1 | 1 | 0 | 1917 |
| graphql | 2 | 2 | 0 | 0 | 11243 |
| grpc | 2 | 2 | 0 | 0 | 531 |
| rest | 2 | 0 | 2 | 0 | 2306 |
| soap | 2 | 2 | 0 | 0 | 675 |

## Per domain

| Domain | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| devtools | 2 | 2 | 0 | 0 | 11243 |
| ecommerce | 3 | 2 | 1 | 0 | 2125 |
| generic | 3 | 3 | 0 | 0 | 675 |
| payments | 2 | 0 | 2 | 0 | 2306 |
