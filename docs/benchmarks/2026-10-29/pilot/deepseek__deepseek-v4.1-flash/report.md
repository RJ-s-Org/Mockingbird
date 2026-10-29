# Mockingbird LLM evals

Model `deepseek/deepseek-v4.1-flash` at `http://127.0.0.1:62241/r/deepseek__deepseek-v4.1-flash~pilot/api/v1`, started 2026-10-29T18:03:26.301473+00:00.

| Metric | Value |
| --- | --- |
| Calls | 10 |
| Schema-valid rate | 100.0% |
| First-pass rate | 90.0% |
| Repair rate | 10.0% |
| Fallback rate | 0.0% |
| p50 latency | 1047 ms |
| p95 latency | 7571 ms |
| Prompt tokens per call | 988.6 |
| Completion tokens per call | 239.7 |

## Per contract

| Contract | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| asyncapi/commerce-events-kafka | 1 | 1 | 0 | 0 | 1892 |
| asyncapi/orders-amqp-2.6 | 1 | 1 | 0 | 0 | 625 |
| graphql/devtools-github | 1 | 1 | 0 | 0 | 7571 |
| graphql/ecommerce-shopify-admin | 1 | 1 | 0 | 0 | 1144 |
| grpc/otlp-trace-service | 1 | 1 | 0 | 0 | 570 |
| grpc/route-guide | 1 | 1 | 0 | 0 | 1047 |
| rest/payments-paypal-orders | 1 | 1 | 0 | 0 | 3194 |
| rest/payments-stripe | 1 | 0 | 1 | 0 | 2165 |
| soap/accounts-rpc | 1 | 1 | 0 | 0 | 907 |
| soap/calculator | 1 | 1 | 0 | 0 | 692 |

## Per API type

| API type | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| asyncapi | 2 | 2 | 0 | 0 | 1892 |
| graphql | 2 | 2 | 0 | 0 | 7571 |
| grpc | 2 | 2 | 0 | 0 | 1047 |
| rest | 2 | 1 | 1 | 0 | 3194 |
| soap | 2 | 2 | 0 | 0 | 907 |

## Per domain

| Domain | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| devtools | 2 | 2 | 0 | 0 | 7571 |
| ecommerce | 3 | 3 | 0 | 0 | 1892 |
| generic | 3 | 3 | 0 | 0 | 1047 |
| payments | 2 | 1 | 1 | 0 | 3194 |
