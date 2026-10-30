# Mockingbird LLM evals

Model `nvidia/nemotron-3.5-super-vl` at `http://127.0.0.1:58482/r/nvidia__nemotron-3.5-super-vl~engine/api/v1`, started 2026-10-30T07:34:30.189404+00:00.

| Metric | Value |
| --- | --- |
| Calls | 162 |
| Schema-valid rate | 97.5% |
| First-pass rate | 54.9% |
| Repair rate | 42.6% |
| Fallback rate | 2.5% |
| p50 latency | 2780 ms |
| p95 latency | 14647 ms |
| Prompt tokens per call | 1247.3 |
| Completion tokens per call | 616.0 |
| Streaming operations not evaluated | 3 |

## Per contract

| Contract | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| asyncapi/commerce-events-kafka | 9 | 5 | 4 | 0 | 6452 |
| asyncapi/orders-amqp-2.6 | 6 | 6 | 0 | 0 | 3234 |
| graphql/devtools-github | 12 | 12 | 0 | 0 | 12547 |
| graphql/ecommerce-shopify-admin | 12 | 12 | 0 | 0 | 12619 |
| grpc/otlp-trace-service | 3 | 3 | 0 | 0 | 531 |
| grpc/route-guide | 3 | 3 | 0 | 0 | 2186 |
| rest/devtools-github | 12 | 0 | 12 | 0 | 16082 |
| rest/ecommerce-orders-3.1 | 9 | 6 | 3 | 0 | 3374 |
| rest/ecommerce-square | 12 | 6 | 6 | 0 | 15181 |
| rest/payments-paypal-orders | 9 | 0 | 8 | 1 | 13639 |
| rest/payments-stripe | 12 | 3 | 9 | 0 | 8466 |
| rest/social-discord | 9 | 2 | 7 | 0 | 13397 |
| rest/social-spotify | 9 | 0 | 9 | 0 | 34806 |
| rest/social-tmdb | 12 | 1 | 8 | 3 | 14895 |
| rest/travel-booking | 12 | 11 | 1 | 0 | 15778 |
| rest/travel-hotels | 6 | 4 | 2 | 0 | 12989 |
| soap/accounts-rpc | 6 | 6 | 0 | 0 | 3222 |
| soap/calculator | 9 | 9 | 0 | 0 | 1655 |

## Per API type

| API type | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| asyncapi | 15 | 11 | 4 | 0 | 6452 |
| graphql | 24 | 24 | 0 | 0 | 12547 |
| grpc | 6 | 6 | 0 | 0 | 2186 |
| rest | 102 | 33 | 65 | 4 | 15451 |
| soap | 15 | 15 | 0 | 0 | 3222 |

## Per domain

| Domain | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| devtools | 27 | 15 | 12 | 0 | 13781 |
| ecommerce | 48 | 35 | 13 | 0 | 12619 |
| generic | 18 | 18 | 0 | 0 | 3222 |
| payments | 21 | 3 | 17 | 1 | 13124 |
| social-media | 30 | 3 | 24 | 3 | 16041 |
| travel | 18 | 15 | 3 | 0 | 15778 |
