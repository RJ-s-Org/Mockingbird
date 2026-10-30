# Mockingbird LLM evals

Model `thinkingmachines/inkling-small@nojson` at `http://127.0.0.1:58482/r/thinkingmachines__inkling-small@nojson~engine/api/v1`, started 2026-10-30T07:53:08.714908+00:00.

| Metric | Value |
| --- | --- |
| Calls | 162 |
| Schema-valid rate | 92.0% |
| First-pass rate | 49.4% |
| Repair rate | 42.6% |
| Fallback rate | 8.0% |
| p50 latency | 3363 ms |
| p95 latency | 26602 ms |
| Prompt tokens per call | 1208.3 |
| Completion tokens per call | 446.1 |
| Streaming operations not evaluated | 3 |

## Per contract

| Contract | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| asyncapi/commerce-events-kafka | 9 | 1 | 8 | 0 | 2210 |
| asyncapi/orders-amqp-2.6 | 6 | 6 | 0 | 0 | 1185 |
| graphql/devtools-github | 12 | 10 | 2 | 0 | 4657 |
| graphql/ecommerce-shopify-admin | 12 | 12 | 0 | 0 | 21667 |
| grpc/otlp-trace-service | 3 | 3 | 0 | 0 | 638 |
| grpc/route-guide | 3 | 3 | 0 | 0 | 451 |
| rest/devtools-github | 12 | 3 | 7 | 2 | 4914 |
| rest/ecommerce-orders-3.1 | 9 | 7 | 1 | 1 | 32509 |
| rest/ecommerce-square | 12 | 7 | 4 | 1 | 24027 |
| rest/payments-paypal-orders | 9 | 0 | 9 | 0 | 34619 |
| rest/payments-stripe | 12 | 0 | 12 | 0 | 30541 |
| rest/social-discord | 9 | 0 | 6 | 3 | 6237 |
| rest/social-spotify | 9 | 0 | 4 | 5 | 20452 |
| rest/social-tmdb | 12 | 0 | 11 | 1 | 6365 |
| rest/travel-booking | 12 | 8 | 4 | 0 | 3261 |
| rest/travel-hotels | 6 | 5 | 1 | 0 | 3483 |
| soap/accounts-rpc | 6 | 6 | 0 | 0 | 738 |
| soap/calculator | 9 | 9 | 0 | 0 | 553 |

## Per API type

| API type | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| asyncapi | 15 | 7 | 8 | 0 | 2210 |
| graphql | 24 | 22 | 2 | 0 | 20343 |
| grpc | 6 | 6 | 0 | 0 | 638 |
| rest | 102 | 30 | 59 | 13 | 29782 |
| soap | 15 | 15 | 0 | 0 | 738 |

## Per domain

| Domain | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| devtools | 27 | 16 | 9 | 2 | 4657 |
| ecommerce | 48 | 33 | 13 | 2 | 29038 |
| generic | 18 | 18 | 0 | 0 | 738 |
| payments | 21 | 0 | 21 | 0 | 32080 |
| social-media | 30 | 0 | 21 | 9 | 19845 |
| travel | 18 | 13 | 5 | 0 | 3483 |
