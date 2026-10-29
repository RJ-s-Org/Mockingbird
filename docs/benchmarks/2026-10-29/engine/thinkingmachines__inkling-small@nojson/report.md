# Mockingbird LLM evals

Model `thinkingmachines/inkling-small@nojson` at `http://127.0.0.1:58357/r/thinkingmachines__inkling-small@nojson~engine/api/v1`, started 2026-10-29T19:45:07.266667+00:00.

| Metric | Value |
| --- | --- |
| Calls | 162 |
| Schema-valid rate | 83.3% |
| First-pass rate | 41.4% |
| Repair rate | 42.0% |
| Fallback rate | 16.7% |
| p50 latency | 1635 ms |
| p95 latency | 7118 ms |
| Prompt tokens per call | 1505.5 |
| Completion tokens per call | 487.2 |

## Per contract

| Contract | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| asyncapi/commerce-events-kafka | 9 | 0 | 8 | 1 | 1910 |
| asyncapi/orders-amqp-2.6 | 6 | 6 | 0 | 0 | 632 |
| graphql/devtools-github | 12 | 3 | 0 | 9 | 10200 |
| graphql/ecommerce-shopify-admin | 12 | 7 | 0 | 5 | 6488 |
| grpc/otlp-trace-service | 3 | 3 | 0 | 0 | 599 |
| grpc/route-guide | 3 | 3 | 0 | 0 | 819 |
| rest/devtools-github | 12 | 1 | 10 | 1 | 4260 |
| rest/ecommerce-orders-3.1 | 9 | 7 | 2 | 0 | 1878 |
| rest/ecommerce-square | 12 | 4 | 8 | 0 | 3894 |
| rest/payments-paypal-orders | 9 | 0 | 8 | 1 | 4569 |
| rest/payments-stripe | 12 | 2 | 10 | 0 | 1257 |
| rest/social-discord | 9 | 0 | 6 | 3 | 5941 |
| rest/social-spotify | 9 | 0 | 3 | 6 | 10481 |
| rest/social-tmdb | 12 | 0 | 11 | 1 | 4145 |
| rest/travel-booking | 12 | 11 | 1 | 0 | 2781 |
| rest/travel-hotels | 6 | 5 | 1 | 0 | 3177 |
| soap/accounts-rpc | 6 | 6 | 0 | 0 | 1241 |
| soap/calculator | 9 | 9 | 0 | 0 | 636 |

## Per API type

| API type | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| asyncapi | 15 | 6 | 8 | 1 | 1910 |
| graphql | 24 | 10 | 0 | 14 | 9589 |
| grpc | 6 | 6 | 0 | 0 | 819 |
| rest | 102 | 30 | 60 | 12 | 6989 |
| soap | 15 | 15 | 0 | 0 | 1241 |

## Per domain

| Domain | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| devtools | 27 | 7 | 10 | 10 | 9589 |
| ecommerce | 48 | 24 | 18 | 6 | 3979 |
| generic | 18 | 18 | 0 | 0 | 1241 |
| payments | 21 | 2 | 18 | 1 | 2575 |
| social-media | 30 | 0 | 20 | 10 | 8387 |
| travel | 18 | 16 | 2 | 0 | 3177 |
