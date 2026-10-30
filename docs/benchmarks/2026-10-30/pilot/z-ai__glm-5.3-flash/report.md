# Mockingbird LLM evals

Model `z-ai/glm-5.3-flash` at `http://127.0.0.1:58282/r/z-ai__glm-5.3-flash~pilot/api/v1`, started 2026-10-30T07:28:39.667568+00:00.

| Metric | Value |
| --- | --- |
| Calls | 10 |
| Schema-valid rate | 100.0% |
| First-pass rate | 90.0% |
| Repair rate | 10.0% |
| Fallback rate | 0.0% |
| p50 latency | 2922 ms |
| p95 latency | 16630 ms |
| Prompt tokens per call | 752.3 |
| Completion tokens per call | 561.9 |
| Streaming operations not evaluated | 3 |

## Per contract

| Contract | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| asyncapi/commerce-events-kafka | 1 | 1 | 0 | 0 | 7539 |
| asyncapi/orders-amqp-2.6 | 1 | 1 | 0 | 0 | 1110 |
| graphql/devtools-github | 1 | 1 | 0 | 0 | 16630 |
| graphql/ecommerce-shopify-admin | 1 | 1 | 0 | 0 | 2894 |
| grpc/otlp-trace-service | 1 | 0 | 1 | 0 | 2922 |
| grpc/route-guide | 1 | 1 | 0 | 0 | 2936 |
| rest/payments-paypal-orders | 1 | 1 | 0 | 0 | 3955 |
| rest/payments-stripe | 1 | 1 | 0 | 0 | 7021 |
| soap/accounts-rpc | 1 | 1 | 0 | 0 | 2168 |
| soap/calculator | 1 | 1 | 0 | 0 | 2313 |

## Per API type

| API type | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| asyncapi | 2 | 2 | 0 | 0 | 7539 |
| graphql | 2 | 2 | 0 | 0 | 16630 |
| grpc | 2 | 1 | 1 | 0 | 2936 |
| rest | 2 | 2 | 0 | 0 | 7021 |
| soap | 2 | 2 | 0 | 0 | 2313 |

## Per domain

| Domain | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| devtools | 2 | 1 | 1 | 0 | 16630 |
| ecommerce | 3 | 3 | 0 | 0 | 7539 |
| generic | 3 | 3 | 0 | 0 | 2936 |
| payments | 2 | 2 | 0 | 0 | 7021 |
