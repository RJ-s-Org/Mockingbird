# Mockingbird LLM evals

Model `inception/mercury-3` at `http://127.0.0.1:58282/r/inception__mercury-3~pilot/api/v1`, started 2026-10-30T07:28:19.857164+00:00.

| Metric | Value |
| --- | --- |
| Calls | 10 |
| Schema-valid rate | 100.0% |
| First-pass rate | 70.0% |
| Repair rate | 30.0% |
| Fallback rate | 0.0% |
| p50 latency | 441 ms |
| p95 latency | 2465 ms |
| Prompt tokens per call | 862.7 |
| Completion tokens per call | 144.1 |
| Streaming operations not evaluated | 3 |

## Per contract

| Contract | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| asyncapi/commerce-events-kafka | 1 | 0 | 1 | 0 | 814 |
| asyncapi/orders-amqp-2.6 | 1 | 1 | 0 | 0 | 406 |
| graphql/devtools-github | 1 | 1 | 0 | 0 | 1184 |
| graphql/ecommerce-shopify-admin | 1 | 1 | 0 | 0 | 672 |
| grpc/otlp-trace-service | 1 | 1 | 0 | 0 | 441 |
| grpc/route-guide | 1 | 1 | 0 | 0 | 399 |
| rest/payments-paypal-orders | 1 | 0 | 1 | 0 | 871 |
| rest/payments-stripe | 1 | 0 | 1 | 0 | 2465 |
| soap/accounts-rpc | 1 | 1 | 0 | 0 | 420 |
| soap/calculator | 1 | 1 | 0 | 0 | 421 |

## Per API type

| API type | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| asyncapi | 2 | 1 | 1 | 0 | 814 |
| graphql | 2 | 2 | 0 | 0 | 1184 |
| grpc | 2 | 2 | 0 | 0 | 441 |
| rest | 2 | 0 | 2 | 0 | 2465 |
| soap | 2 | 2 | 0 | 0 | 421 |

## Per domain

| Domain | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| devtools | 2 | 2 | 0 | 0 | 1184 |
| ecommerce | 3 | 2 | 1 | 0 | 814 |
| generic | 3 | 3 | 0 | 0 | 421 |
| payments | 2 | 0 | 2 | 0 | 2465 |
