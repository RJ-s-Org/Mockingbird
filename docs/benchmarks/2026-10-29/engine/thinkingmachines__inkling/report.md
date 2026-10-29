# Mockingbird LLM evals

Model `thinkingmachines/inkling` at `http://127.0.0.1:58357/r/thinkingmachines__inkling~engine/api/v1`, started 2026-10-29T19:54:43.418485+00:00.

| Metric | Value |
| --- | --- |
| Calls | 162 |
| Schema-valid rate | 0.0% |
| First-pass rate | 0.0% |
| Repair rate | 0.0% |
| Fallback rate | 100.0% |
| p50 latency | 8369 ms |
| p95 latency | 28368 ms |
| Prompt tokens per call | 2499.1 |
| Completion tokens per call | 934.6 |

## Per contract

| Contract | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| asyncapi/commerce-events-kafka | 9 | 0 | 0 | 9 | 9790 |
| asyncapi/orders-amqp-2.6 | 6 | 0 | 0 | 6 | 2223 |
| graphql/devtools-github | 12 | 0 | 0 | 12 | 37455 |
| graphql/ecommerce-shopify-admin | 12 | 0 | 0 | 12 | 44343 |
| grpc/otlp-trace-service | 3 | 0 | 0 | 3 | 1662 |
| grpc/route-guide | 3 | 0 | 0 | 3 | 1657 |
| rest/devtools-github | 12 | 0 | 0 | 12 | 24733 |
| rest/ecommerce-orders-3.1 | 9 | 0 | 0 | 9 | 7748 |
| rest/ecommerce-square | 12 | 0 | 0 | 12 | 21881 |
| rest/payments-paypal-orders | 9 | 0 | 0 | 9 | 14562 |
| rest/payments-stripe | 12 | 0 | 0 | 12 | 6179 |
| rest/social-discord | 9 | 0 | 0 | 9 | 22313 |
| rest/social-spotify | 9 | 0 | 0 | 9 | 30989 |
| rest/social-tmdb | 12 | 0 | 0 | 12 | 35879 |
| rest/travel-booking | 12 | 0 | 0 | 12 | 28368 |
| rest/travel-hotels | 6 | 0 | 0 | 6 | 29753 |
| soap/accounts-rpc | 6 | 0 | 0 | 6 | 3444 |
| soap/calculator | 9 | 0 | 0 | 9 | 1226 |

## Per API type

| API type | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| asyncapi | 15 | 0 | 0 | 15 | 9790 |
| graphql | 24 | 0 | 0 | 24 | 37455 |
| grpc | 6 | 0 | 0 | 6 | 1662 |
| rest | 102 | 0 | 0 | 102 | 27050 |
| soap | 15 | 0 | 0 | 15 | 3444 |

## Per domain

| Domain | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| devtools | 27 | 0 | 0 | 27 | 35271 |
| ecommerce | 48 | 0 | 0 | 48 | 23121 |
| generic | 18 | 0 | 0 | 18 | 3444 |
| payments | 21 | 0 | 0 | 21 | 14172 |
| social-media | 30 | 0 | 0 | 30 | 30989 |
| travel | 18 | 0 | 0 | 18 | 29753 |
