# Mockingbird LLM evals

Model `minimax/minimax-m3.1-flash` at `http://127.0.0.1:62384/r/minimax__minimax-m3.1-flash~engine/api/v1`, started 2026-10-29T18:23:03.879242+00:00.

| Metric | Value |
| --- | --- |
| Calls | 162 |
| Schema-valid rate | 87.0% |
| First-pass rate | 45.1% |
| Repair rate | 42.0% |
| Fallback rate | 13.0% |
| p50 latency | 1953 ms |
| p95 latency | 13257 ms |
| Prompt tokens per call | 1379.3 |
| Completion tokens per call | 387.3 |

## Per contract

| Contract | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| asyncapi/commerce-events-kafka | 9 | 3 | 6 | 0 | 5370 |
| asyncapi/orders-amqp-2.6 | 6 | 6 | 0 | 0 | 3788 |
| graphql/devtools-github | 12 | 1 | 2 | 9 | 11734 |
| graphql/ecommerce-shopify-admin | 12 | 8 | 2 | 2 | 4728 |
| grpc/otlp-trace-service | 3 | 3 | 0 | 0 | 2661 |
| grpc/route-guide | 3 | 3 | 0 | 0 | 619 |
| rest/devtools-github | 12 | 2 | 10 | 0 | 6066 |
| rest/ecommerce-orders-3.1 | 9 | 9 | 0 | 0 | 3714 |
| rest/ecommerce-square | 12 | 7 | 5 | 0 | 4981 |
| rest/payments-paypal-orders | 9 | 0 | 8 | 1 | 10450 |
| rest/payments-stripe | 12 | 3 | 9 | 0 | 13523 |
| rest/social-discord | 9 | 4 | 2 | 3 | 27143 |
| rest/social-spotify | 9 | 0 | 7 | 2 | 19653 |
| rest/social-tmdb | 12 | 1 | 9 | 2 | 30004 |
| rest/travel-booking | 12 | 6 | 5 | 1 | 14016 |
| rest/travel-hotels | 6 | 2 | 3 | 1 | 16197 |
| soap/accounts-rpc | 6 | 6 | 0 | 0 | 11264 |
| soap/calculator | 9 | 9 | 0 | 0 | 2375 |

## Per API type

| API type | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| asyncapi | 15 | 9 | 6 | 0 | 5370 |
| graphql | 24 | 9 | 4 | 11 | 11200 |
| grpc | 6 | 6 | 0 | 0 | 2661 |
| rest | 102 | 34 | 58 | 10 | 16197 |
| soap | 15 | 15 | 0 | 0 | 11264 |

## Per domain

| Domain | Calls | LLM | Repaired | Fallback | p95 ms |
| --- | --- | --- | --- | --- | --- |
| devtools | 27 | 6 | 12 | 9 | 11200 |
| ecommerce | 48 | 33 | 13 | 2 | 4981 |
| generic | 18 | 18 | 0 | 0 | 11264 |
| payments | 21 | 3 | 17 | 1 | 10450 |
| social-media | 30 | 5 | 18 | 7 | 27143 |
| travel | 18 | 8 | 8 | 2 | 16197 |
