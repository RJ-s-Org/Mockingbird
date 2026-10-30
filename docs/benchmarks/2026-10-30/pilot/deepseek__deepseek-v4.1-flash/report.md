# Mockingbird LLM evals

Model `deepseek/deepseek-v4.1-flash` at `http://127.0.0.1:58282/r/deepseek__deepseek-v4.1-flash~pilot/api/v1`, started 2026-10-30T07:27:49.314756+00:00.

| Metric | Value |
| --- | --- |
| Calls | 10 |
| Schema-valid rate | 100.0% |
| First-pass rate | 90.0% |
| Repair rate | 10.0% |
| Fallback rate | 0.0% |
| p50 latency | 767 ms |
| p95 latency | 5516 ms |
| Prompt tokens per call | 723.3 |
| Completion tokens per call | 166.5 |
| Streaming operations not evaluated | 3 |

## Per contract

| Contract | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| asyncapi/commerce-events-kafka | 1 | 1 | 0 | 0 | 981 |
| asyncapi/orders-amqp-2.6 | 1 | 1 | 0 | 0 | 529 |
| graphql/devtools-github | 1 | 1 | 0 | 0 | 5516 |
| graphql/ecommerce-shopify-admin | 1 | 1 | 0 | 0 | 735 |
| grpc/otlp-trace-service | 1 | 1 | 0 | 0 | 534 |
| grpc/route-guide | 1 | 1 | 0 | 0 | 1367 |
| rest/payments-paypal-orders | 1 | 1 | 0 | 0 | 3683 |
| rest/payments-stripe | 1 | 0 | 1 | 0 | 5242 |
| soap/accounts-rpc | 1 | 1 | 0 | 0 | 767 |
| soap/calculator | 1 | 1 | 0 | 0 | 631 |

## Per API type

| API type | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| asyncapi | 2 | 2 | 0 | 0 | 981 |
| graphql | 2 | 2 | 0 | 0 | 5516 |
| grpc | 2 | 2 | 0 | 0 | 1367 |
| rest | 2 | 1 | 1 | 0 | 5242 |
| soap | 2 | 2 | 0 | 0 | 767 |

## Per domain

| Domain | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| devtools | 2 | 2 | 0 | 0 | 5516 |
| ecommerce | 3 | 3 | 0 | 0 | 981 |
| generic | 3 | 3 | 0 | 0 | 1367 |
| payments | 2 | 1 | 1 | 0 | 5242 |
