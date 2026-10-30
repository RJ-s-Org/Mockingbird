# Mockingbird LLM evals

Model `openai/gpt-6-luna` at `http://127.0.0.1:58282/r/openai__gpt-6-luna~pilot/api/v1`, started 2026-10-30T07:29:31.027197+00:00.

| Metric | Value |
| --- | --- |
| Calls | 10 |
| Schema-valid rate | 100.0% |
| First-pass rate | 90.0% |
| Repair rate | 10.0% |
| Fallback rate | 0.0% |
| p50 latency | 1643 ms |
| p95 latency | 8308 ms |
| Prompt tokens per call | 679.7 |
| Completion tokens per call | 163.2 |
| Streaming operations not evaluated | 3 |

## Per contract

| Contract | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| asyncapi/commerce-events-kafka | 1 | 1 | 0 | 0 | 2656 |
| asyncapi/orders-amqp-2.6 | 1 | 1 | 0 | 0 | 1107 |
| graphql/devtools-github | 1 | 1 | 0 | 0 | 5007 |
| graphql/ecommerce-shopify-admin | 1 | 1 | 0 | 0 | 1959 |
| grpc/otlp-trace-service | 1 | 1 | 0 | 0 | 1294 |
| grpc/route-guide | 1 | 1 | 0 | 0 | 1312 |
| rest/payments-paypal-orders | 1 | 1 | 0 | 0 | 8308 |
| rest/payments-stripe | 1 | 0 | 1 | 0 | 2853 |
| soap/accounts-rpc | 1 | 1 | 0 | 0 | 1643 |
| soap/calculator | 1 | 1 | 0 | 0 | 1035 |

## Per API type

| API type | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| asyncapi | 2 | 2 | 0 | 0 | 2656 |
| graphql | 2 | 2 | 0 | 0 | 5007 |
| grpc | 2 | 2 | 0 | 0 | 1312 |
| rest | 2 | 1 | 1 | 0 | 8308 |
| soap | 2 | 2 | 0 | 0 | 1643 |

## Per domain

| Domain | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| devtools | 2 | 2 | 0 | 0 | 5007 |
| ecommerce | 3 | 3 | 0 | 0 | 2656 |
| generic | 3 | 3 | 0 | 0 | 1643 |
| payments | 2 | 1 | 1 | 0 | 8308 |
