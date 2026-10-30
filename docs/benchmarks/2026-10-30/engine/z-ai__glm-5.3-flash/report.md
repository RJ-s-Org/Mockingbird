# Mockingbird LLM evals

Model `z-ai/glm-5.3-flash` at `http://127.0.0.1:58482/r/z-ai__glm-5.3-flash~engine/api/v1`, started 2026-10-30T07:36:07.548471+00:00.

| Metric | Value |
| --- | --- |
| Calls | 162 |
| Schema-valid rate | 99.4% |
| First-pass rate | 63.6% |
| Repair rate | 35.8% |
| Fallback rate | 0.6% |
| p50 latency | 4414 ms |
| p95 latency | 26503 ms |
| Prompt tokens per call | 1102.7 |
| Completion tokens per call | 1223.9 |
| Streaming operations not evaluated | 3 |

## Per contract

| Contract | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| asyncapi/commerce-events-kafka | 9 | 4 | 5 | 0 | 21349 |
| asyncapi/orders-amqp-2.6 | 6 | 6 | 0 | 0 | 572 |
| graphql/devtools-github | 12 | 12 | 0 | 0 | 9945 |
| graphql/ecommerce-shopify-admin | 12 | 10 | 2 | 0 | 30528 |
| grpc/otlp-trace-service | 3 | 3 | 0 | 0 | 3959 |
| grpc/route-guide | 3 | 3 | 0 | 0 | 2651 |
| rest/devtools-github | 12 | 3 | 8 | 1 | 28862 |
| rest/ecommerce-orders-3.1 | 9 | 7 | 2 | 0 | 4633 |
| rest/ecommerce-square | 12 | 8 | 4 | 0 | 25771 |
| rest/payments-paypal-orders | 9 | 3 | 6 | 0 | 18425 |
| rest/payments-stripe | 12 | 7 | 5 | 0 | 22106 |
| rest/social-discord | 9 | 4 | 5 | 0 | 18075 |
| rest/social-spotify | 9 | 6 | 3 | 0 | 31934 |
| rest/social-tmdb | 12 | 3 | 9 | 0 | 37386 |
| rest/travel-booking | 12 | 8 | 4 | 0 | 44161 |
| rest/travel-hotels | 6 | 1 | 5 | 0 | 7455 |
| soap/accounts-rpc | 6 | 6 | 0 | 0 | 3228 |
| soap/calculator | 9 | 9 | 0 | 0 | 2303 |

## Per API type

| API type | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| asyncapi | 15 | 10 | 5 | 0 | 21349 |
| graphql | 24 | 22 | 2 | 0 | 16238 |
| grpc | 6 | 6 | 0 | 0 | 3959 |
| rest | 102 | 50 | 51 | 1 | 28213 |
| soap | 15 | 15 | 0 | 0 | 3228 |

## Per domain

| Domain | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| devtools | 27 | 18 | 8 | 1 | 21160 |
| ecommerce | 48 | 35 | 13 | 0 | 21349 |
| generic | 18 | 18 | 0 | 0 | 3228 |
| payments | 21 | 10 | 11 | 0 | 18425 |
| social-media | 30 | 13 | 17 | 0 | 31934 |
| travel | 18 | 9 | 9 | 0 | 44161 |
