# Mockingbird LLM evals

Model `deepseek/deepseek-v4.1-flash-1017` at `http://127.0.0.1:58282/r/deepseek__deepseek-v4.1-flash-1017~pilot/api/v1`, started 2026-10-30T07:27:56.221472+00:00.

| Metric | Value |
| --- | --- |
| Calls | 10 |
| Schema-valid rate | 100.0% |
| First-pass rate | 90.0% |
| Repair rate | 10.0% |
| Fallback rate | 0.0% |
| p50 latency | 582 ms |
| p95 latency | 3483 ms |
| Prompt tokens per call | 720.6 |
| Completion tokens per call | 148.3 |
| Streaming operations not evaluated | 3 |

## Per contract

| Contract | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| asyncapi/commerce-events-kafka | 1 | 1 | 0 | 0 | 3483 |
| asyncapi/orders-amqp-2.6 | 1 | 1 | 0 | 0 | 487 |
| graphql/devtools-github | 1 | 1 | 0 | 0 | 1441 |
| graphql/ecommerce-shopify-admin | 1 | 1 | 0 | 0 | 582 |
| grpc/otlp-trace-service | 1 | 1 | 0 | 0 | 355 |
| grpc/route-guide | 1 | 1 | 0 | 0 | 382 |
| rest/payments-paypal-orders | 1 | 1 | 0 | 0 | 1035 |
| rest/payments-stripe | 1 | 0 | 1 | 0 | 945 |
| soap/accounts-rpc | 1 | 1 | 0 | 0 | 468 |
| soap/calculator | 1 | 1 | 0 | 0 | 1239 |

## Per API type

| API type | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| asyncapi | 2 | 2 | 0 | 0 | 3483 |
| graphql | 2 | 2 | 0 | 0 | 1441 |
| grpc | 2 | 2 | 0 | 0 | 382 |
| rest | 2 | 1 | 1 | 0 | 1035 |
| soap | 2 | 2 | 0 | 0 | 1239 |

## Per domain

| Domain | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| devtools | 2 | 2 | 0 | 0 | 1441 |
| ecommerce | 3 | 3 | 0 | 0 | 3483 |
| generic | 3 | 3 | 0 | 0 | 1239 |
| payments | 2 | 1 | 1 | 0 | 1035 |
