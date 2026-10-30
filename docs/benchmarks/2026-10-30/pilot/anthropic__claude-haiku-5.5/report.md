# Mockingbird LLM evals

Model `anthropic/claude-haiku-5.5` at `http://127.0.0.1:58282/r/anthropic__claude-haiku-5.5~pilot/api/v1`, started 2026-10-30T07:28:22.348916+00:00.

| Metric | Value |
| --- | --- |
| Calls | 10 |
| Schema-valid rate | 100.0% |
| First-pass rate | 90.0% |
| Repair rate | 10.0% |
| Fallback rate | 0.0% |
| p50 latency | 946 ms |
| p95 latency | 3571 ms |
| Prompt tokens per call | 1103.6 |
| Completion tokens per call | 220.9 |
| Streaming operations not evaluated | 3 |

## Per contract

| Contract | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| asyncapi/commerce-events-kafka | 1 | 1 | 0 | 0 | 2262 |
| asyncapi/orders-amqp-2.6 | 1 | 1 | 0 | 0 | 873 |
| graphql/devtools-github | 1 | 1 | 0 | 0 | 3571 |
| graphql/ecommerce-shopify-admin | 1 | 1 | 0 | 0 | 1164 |
| grpc/otlp-trace-service | 1 | 1 | 0 | 0 | 718 |
| grpc/route-guide | 1 | 1 | 0 | 0 | 781 |
| rest/payments-paypal-orders | 1 | 1 | 0 | 0 | 2146 |
| rest/payments-stripe | 1 | 0 | 1 | 0 | 1443 |
| soap/accounts-rpc | 1 | 1 | 0 | 0 | 946 |
| soap/calculator | 1 | 1 | 0 | 0 | 770 |

## Per API type

| API type | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| asyncapi | 2 | 2 | 0 | 0 | 2262 |
| graphql | 2 | 2 | 0 | 0 | 3571 |
| grpc | 2 | 2 | 0 | 0 | 781 |
| rest | 2 | 1 | 1 | 0 | 2146 |
| soap | 2 | 2 | 0 | 0 | 946 |

## Per domain

| Domain | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| devtools | 2 | 2 | 0 | 0 | 3571 |
| ecommerce | 3 | 3 | 0 | 0 | 2262 |
| generic | 3 | 3 | 0 | 0 | 946 |
| payments | 2 | 1 | 1 | 0 | 2146 |
