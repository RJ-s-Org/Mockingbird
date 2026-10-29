# Mockingbird LLM evals

Model `thinkingmachines/inkling@nojson` at `http://127.0.0.1:58357/r/thinkingmachines__inkling@nojson~engine/api/v1`, started 2026-10-29T19:51:16.768227+00:00.

| Metric | Value |
| --- | --- |
| Calls | 162 |
| Schema-valid rate | 91.4% |
| First-pass rate | 47.5% |
| Repair rate | 43.8% |
| Fallback rate | 8.6% |
| p50 latency | 2397 ms |
| p95 latency | 13025 ms |
| Prompt tokens per call | 1531.3 |
| Completion tokens per call | 384.8 |

## Per contract

| Contract | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| asyncapi/commerce-events-kafka | 9 | 0 | 9 | 0 | 2529 |
| asyncapi/orders-amqp-2.6 | 6 | 6 | 0 | 0 | 544 |
| graphql/devtools-github | 12 | 3 | 2 | 7 | 26356 |
| graphql/ecommerce-shopify-admin | 12 | 9 | 0 | 3 | 9368 |
| grpc/otlp-trace-service | 3 | 3 | 0 | 0 | 644 |
| grpc/route-guide | 3 | 3 | 0 | 0 | 400 |
| rest/devtools-github | 12 | 2 | 10 | 0 | 11908 |
| rest/ecommerce-orders-3.1 | 9 | 5 | 4 | 0 | 2847 |
| rest/ecommerce-square | 12 | 10 | 2 | 0 | 3196 |
| rest/payments-paypal-orders | 9 | 0 | 9 | 0 | 4785 |
| rest/payments-stripe | 12 | 0 | 12 | 0 | 1563 |
| rest/social-discord | 9 | 3 | 3 | 3 | 12080 |
| rest/social-spotify | 9 | 2 | 6 | 1 | 21053 |
| rest/social-tmdb | 12 | 0 | 12 | 0 | 7520 |
| rest/travel-booking | 12 | 12 | 0 | 0 | 8508 |
| rest/travel-hotels | 6 | 4 | 2 | 0 | 5383 |
| soap/accounts-rpc | 6 | 6 | 0 | 0 | 927 |
| soap/calculator | 9 | 9 | 0 | 0 | 621 |

## Per API type

| API type | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| asyncapi | 15 | 6 | 9 | 0 | 2529 |
| graphql | 24 | 12 | 2 | 10 | 23686 |
| grpc | 6 | 6 | 0 | 0 | 644 |
| rest | 102 | 38 | 60 | 4 | 11908 |
| soap | 15 | 15 | 0 | 0 | 927 |

## Per domain

| Domain | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| devtools | 27 | 8 | 12 | 7 | 23686 |
| ecommerce | 48 | 30 | 15 | 3 | 7475 |
| generic | 18 | 18 | 0 | 0 | 927 |
| payments | 21 | 0 | 21 | 0 | 4432 |
| social-media | 30 | 5 | 21 | 4 | 16473 |
| travel | 18 | 16 | 2 | 0 | 8508 |
